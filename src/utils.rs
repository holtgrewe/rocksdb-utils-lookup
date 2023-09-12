//! The implementation of the library.

use std::{path::Path, time::Instant};

use crate::error;

/// Tune `RocksDB` options for bulk insertion.
///
/// # Arguments
///
/// * `options` - `RocksDB` options to tune.
/// * `wal_dir` - Optional directory for write-ahead log files.
///
/// # Returns
///
/// Tuned `RocksDB` options.
pub fn tune_options(options: rocksdb::Options, wal_dir: Option<&str>) -> rocksdb::Options {
    let mut options = options;

    options.create_if_missing(true);
    options.create_missing_column_families(true);

    options.prepare_for_bulk_load();

    options.set_max_background_jobs(16);
    options.set_max_subcompactions(8);
    options.increase_parallelism(8);
    options.optimize_level_style_compaction(1 << 30);
    options.set_min_write_buffer_number(1);
    options.set_min_write_buffer_number_to_merge(1);
    options.set_write_buffer_size(1 << 30);
    options.set_target_file_size_base(1 << 30);
    options.set_compaction_style(rocksdb::DBCompactionStyle::Universal);

    if let Some(wal_dir) = wal_dir {
        options.set_wal_dir(wal_dir);
    }

    // Compress everything with zstd.
    options.set_compression_per_level(&[]);
    options.set_bottommost_compression_options(-14, 10, 0, 1 << 14, true);
    options.set_bottommost_compression_type(rocksdb::DBCompressionType::Zstd);
    options.set_bottommost_zstd_max_train_bytes(1 << 22, true);
    options.optimize_for_point_lookup(1 << 26);

    // Setup partitioned index filters
    let mut block_opts = rocksdb::BlockBasedOptions::default();
    block_opts.set_index_type(rocksdb::BlockBasedIndexType::TwoLevelIndexSearch);
    // 10 bits per key are a reasonbel default
    //
    // https://github.com/facebook/rocksdb/wiki/RocksDB-Bloom-Filter
    // https://www.percona.com/blog/how-bloom-filters-work-in-myrocks/
    block_opts.set_bloom_filter(10.0, false);
    block_opts.set_partition_filters(true);
    block_opts.set_metadata_block_size(4096);
    block_opts.set_cache_index_and_filter_blocks(true);
    block_opts.set_pin_top_level_index_and_filter(true);
    block_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
    // MISSING: cache_index_and_filter_blocks_with_high_priority
    options.set_block_based_table_factory(&block_opts);

    options
}

/// Force manual compaction of all column families at the given path.
///
/// This function will enumerate all column families and start a compaction of all of them.
/// It will then wait for the completion of all such compactions.
///
/// # Arguments
///
/// * `path` - Path to the `RocksDB` database.
/// * `options` - `RocksDB` options to use for opening database and column families.
/// * `wait_msg_prefix` - Optional prefix for the wait message.
///
/// # Errors
///
/// Returns an error in the case the underlying `RocksDB` operation fails.
pub fn force_compaction<P>(
    path: P,
    options: &rocksdb::Options,
    wait_msg_prefix: Option<&str>,
) -> Result<(), error::Error>
where
    P: AsRef<Path>,
{
    let cf_names = rocksdb::DB::list_cf(options, path.as_ref())
        .map_err(|e| error::Error::Open(path.as_ref().to_owned(), e))?;
    let cfs = cf_names
        .iter()
        .map(|s| (s, options.clone()))
        .collect::<Vec<_>>();
    let db = rocksdb::DB::open_cf_with_opts(options, path.as_ref(), cfs)
        .map_err(|e| error::Error::Open(path.as_ref().to_owned(), e))?;

    let cf_names_str = cf_names
        .iter()
        .map(std::string::String::as_str)
        .collect::<Vec<_>>();
    force_compaction_cf(&db, cf_names_str, wait_msg_prefix, true)
}

/// Force manual compaction of the given column families in the given database.
///
/// The function will enforce compaction of the bottommost level of all column families.
/// The compression will depend on the options that the database was opened with.  Using the
/// `tune_options` function is recommended to optimize the resulting database.
///
/// Note that you should only set `remove_empty_wal_files` to `true` if you are sure that
/// you close the database just after compaction.
///
/// # Arguments
///
/// * `db` - `RocksDB` database to compact.
/// * `cf_names` - Names of the column families to compact.
/// * `wait_msg_prefix` - Optional prefix for the wait message.
/// * `remove_empty_wal_files` - Whether to remove empty write-ahead log files after compaction.
///
/// # Errors
///
/// Returns an error in the case the underlying `RocksDB` operation fails.
///
/// # Panics
///
/// When there are problems with file system access.
pub fn force_compaction_cf<I, N>(
    db: &rocksdb::DBWithThreadMode<rocksdb::MultiThreaded>,
    cf_names: I,
    wait_msg_prefix: Option<&str>,
    remove_empty_wal_files: bool,
) -> Result<(), error::Error>
where
    I: IntoIterator<Item = N>,
    N: AsRef<str>,
{
    // Collect columns families to run compaction for.
    let cfs = cf_names
        .into_iter()
        .map(|cf| {
            db.cf_handle(cf.as_ref())
                .ok_or(error::Error::ColumnFamily(cf.as_ref().to_owned()))
        })
        .collect::<Result<Vec<_>, error::Error>>()?;

    // Create compaction options and enforce bottommost level compaction.
    let mut compact_opt = rocksdb::CompactOptions::default();
    compact_opt.set_exclusive_manual_compaction(true);
    compact_opt.set_bottommost_level_compaction(rocksdb::BottommostLevelCompaction::Force);

    // Start the compaction for each column family.
    cfs.iter()
        .for_each(|cf| db.compact_range_cf_opt(cf, None::<&[u8]>, None::<&[u8]>, &compact_opt));
    let compaction_start = Instant::now();
    let mut last_logged = compaction_start;

    // Wait until all compactions are done.
    while db
        .property_int_value(rocksdb::properties::COMPACTION_PENDING)
        .map_err(error::Error::PropertyAccess)?
        .ok_or(error::Error::PropertyNotSet(String::from(
            "COMPACTION_PENDING",
        )))?
        > 0
        || db
            .property_int_value(rocksdb::properties::NUM_RUNNING_COMPACTIONS)
            .map_err(error::Error::PropertyAccess)?
            .ok_or(error::Error::PropertyNotSet(String::from(
                "NUM_RUNNING_COMPACTIONS",
            )))?
            > 0
    {
        std::thread::sleep(std::time::Duration::from_millis(100));
        // Log to info every second that compaction is still running.
        if let Some(wait_msg_prefix) = wait_msg_prefix {
            if last_logged.elapsed() > std::time::Duration::from_millis(1000) {
                tracing::info!(
                    "{}still waiting for RocksDB compaction (since {:?})",
                    wait_msg_prefix,
                    compaction_start.elapsed()
                );
                last_logged = Instant::now();
            }
        }
    }

    if remove_empty_wal_files {
        // Remove empty `*.log` files in the database directory.
        let entries = std::fs::read_dir(db.path()).map_err(error::Error::WalRemoval)?;
        for entry in entries {
            let entry = entry.expect("cannot read directory entry");
            if entry.path().extension() == Some(std::ffi::OsStr::new("log"))
                && entry.metadata().map_err(error::Error::WalRemoval)?.len() == 0
            {
                std::fs::remove_file(entry.path()).map_err(error::Error::WalRemoval)?;
            }
        }
    }

    Ok(())
}

/// Function to fetch a meta value as a string from a `RocksDB`.
///
/// # Errors
///
/// Returns an error in the case of problems with the `RocksDB` access.
pub fn fetch_meta(
    db: &rocksdb::DBWithThreadMode<rocksdb::MultiThreaded>,
    key: &str,
) -> Result<Option<String>, error::Error> {
    let cf_meta = db
        .cf_handle("meta")
        .ok_or(error::Error::UnknownColumnFamily)?;
    let raw_data = db
        .get_cf(&cf_meta, key.as_bytes())
        .map_err(error::Error::ReadData)?;
    raw_data
        .map(|raw_data| String::from_utf8(raw_data).map_err(error::Error::InvalidUtf8))
        .transpose()
}

#[allow(clippy::pedantic)]
#[cfg(test)]
mod test {
    use temp_testdir::TempDir;

    use super::*;

    /// Smoke test for the `tune_options` function.
    #[test]
    fn smoke_test_tune_options() -> Result<(), anyhow::Error> {
        let options = rocksdb::Options::default();
        let _tuned = tune_options(options, None);

        Ok(())
    }

    /// Smoke test for the `force_compaction` function.
    #[test]
    fn smoke_test_force_compaction() -> Result<(), anyhow::Error> {
        let temp = TempDir::default();
        let path_db = temp.join("rocksdb");

        let mut options = rocksdb::Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        {
            let cf_names = &["foo", "bar"];
            let _db = rocksdb::DB::open_cf(&options, &path_db, cf_names)?;
        }

        force_compaction(&path_db, &options, Some("msg"))?;

        Ok(())
    }

    /// Smoke test for the `force_compaction` function.
    #[test]
    fn smoke_test_force_compaction_cf() -> Result<(), anyhow::Error> {
        let temp = TempDir::default();
        let path_db = temp.join("rocksdb");

        let mut options = rocksdb::Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        let cf_names = &["foo", "bar"];
        let db = rocksdb::DB::open_cf(&options, path_db, cf_names)?;

        force_compaction_cf(&db, cf_names, Some("msg"), true)?;

        Ok(())
    }

    /// Smoke test for the `fetch_meta` function.
    #[test]
    fn smoke_test_fetch_meta() -> Result<(), anyhow::Error> {
        let path_db = "tests/data/freqs";
        let db = rocksdb::DB::open_cf_for_read_only(
            &rocksdb::Options::default(),
            path_db,
            ["meta"],
            true,
        )?;

        fetch_meta(&db, "gnomad-release")?;

        Ok(())
    }
}
