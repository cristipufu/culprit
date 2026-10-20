//! The `slow-storage` check: Average read and write latency per database file since startup: sys.dm_io_virtual_file_stats, pg_stat_io (16+, with track_io_timing), performance_schema file I/O.

use super::*;
use crate::evidence::Requirement as Evidence;
use std::collections::BTreeSet;

pub fn slow_storage_info() -> CheckInfo {
    CheckInfo {
        id: "slow-storage".into(),
        title: "Storage that's slow to read or write".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Average read and write latency per database file since startup: sys.dm_io_virtual_file_stats, pg_stat_io (16+, with track_io_timing), performance_schema file I/O.".into(),
    }
}

check!(
    SlowStorageCheck: slow_storage_info, slow_storage,
    &[Evidence::Pressure { signal: None, timed_files: true, complete: true }]
);

/// Reads (or log writes) at least this large on average are bulk I/O: backups, CHECKDB, read-ahead of large scans. Each
/// takes longer than the page-sized reads queries wait on, so their average says little about the storage.
pub(super) const BULK_IO_BYTES: f64 = 64.0 * 1024.0;

/// The files of one kind in one database, added up for the operation that matters for that kind: reads of data and
/// temporary files, writes of the log (every commit waits for them).
pub(super) struct FileGroup<'a> {
    files: Vec<&'a FileIo>,
    database: Option<String>,
    kind: FileKind,
    write: bool,
    ops: f64,
    ms: f64,
    /// Bytes moved, when every file says.
    bytes: Option<f64>,
}

impl FileGroup<'_> {
    /// Average time per operation, weighted by operations across the files: one quiet file read in large chunks by a
    /// backup doesn't stand for the database's storage.
    fn avg_ms(&self) -> f64 {
        per_run(self.ms, self.ops)
    }

    /// Average bytes per operation, when known.
    fn avg_bytes(&self) -> Option<f64> {
        self.bytes.map(|b| per_run(b, self.ops))
    }

    fn bulk(&self) -> bool {
        self.avg_bytes().is_some_and(|b| b >= BULK_IO_BYTES)
    }
}

/// Every kind of file in every database whose operation-weighted average crosses its threshold over 1,000 operations.
/// tempdb's log is left out: SQL Server doesn't harden it at commit, so no commit waits for those writes.
pub(super) fn slow_groups<'a>(snapshot: &Snapshot, files: &'a [FileIo]) -> Vec<FileGroup<'a>> {
    let mut groups: BTreeMap<(u8, Option<String>), FileGroup<'a>> = BTreeMap::new();
    for f in files {
        let (order, write) = match f.kind {
            FileKind::Data => (0, false),
            FileKind::Temp => (1, false),
            FileKind::Log => (2, true),
            FileKind::Other => continue,
        };
        if snapshot.server.engine == Engine::Mssql && f.kind == FileKind::Log && f.database.as_deref() == Some("tempdb")
        {
            continue;
        }
        let (ops, ms, bytes) =
            if write { (f.writes, f.write_ms, f.write_bytes) } else { (f.reads, f.read_ms, f.read_bytes) };
        // An operation nobody timed says nothing about its time.
        let Some(ms) = ms.filter(|_| ops > 0.0) else { continue };
        let g = groups.entry((order, f.database.clone())).or_insert_with(|| FileGroup {
            files: Vec::new(),
            database: f.database.clone(),
            kind: f.kind,
            write,
            ops: 0.0,
            ms: 0.0,
            bytes: Some(0.0),
        });
        g.files.push(f);
        g.ops += ops;
        g.ms += ms;
        g.bytes = g.bytes.zip(bytes).map(|(a, b)| a + b);
    }
    groups.into_values().filter(|g| g.ops >= 1_000.0 && g.avg_ms() >= if g.write { 10.0 } else { 20.0 }).collect()
}

pub(super) fn file_label(snapshot: &Snapshot, g: &FileGroup) -> String {
    let one = (g.files.len() == 1).then(|| g.files[0].name.clone());
    match (snapshot.server.engine, g.kind, g.database.as_deref()) {
        (Engine::Mssql, _, Some("tempdb")) => "tempdb".into(),
        (Engine::Mssql, FileKind::Log, Some(db)) => format!("{db}'s log"),
        (Engine::Mssql, _, Some(db)) => match one {
            Some(name) => format!("{db}'s data file {name}"),
            None => format!("{db}'s data files"),
        },
        (Engine::Postgres, FileKind::Log, _) => "WAL".into(),
        (Engine::Postgres, FileKind::Temp, _) => "temporary tables".into(),
        (Engine::Postgres, _, _) => "table and index files".into(),
        (_, kind, _) => one.unwrap_or_else(|| {
            match kind {
                FileKind::Temp => "temporary files",
                FileKind::Log => "the log files",
                _ => "data files",
            }
            .into()
        }),
    }
}

/// How storage that answers slowly is fixed depends on where it runs.
pub(super) fn storage_fix(snapshot: &Snapshot) -> &'static str {
    match snapshot.server.platform() {
        // Azure SQL Database runs on storage you don't manage: the tier decides the latency and the I/O limits.
        Some(culprit_core::Platform::AzureSqlDatabase) => {
            "In Azure SQL Database the service tier decides this: General Purpose reads from remote storage (5 to 10 ms is normal there), Hyperscale serves most reads from a local cache and the rest from page servers, Business Critical uses local SSDs. Check avg_data_io_percent and avg_log_write_percent in sys.dm_db_resource_stats: near 100% means the tier's I/O limit is throttling, so add vCores or move to a faster tier; well below it, cut the reads instead (indexes, fewer full scans). A since-startup average hides bursts: sample Waits → Right now during the busy hour."
        }
        // Managed Instance General Purpose keeps each file on a remote disk sized from the file.
        Some(culprit_core::Platform::AzureSqlManagedInstance) => {
            "On Managed Instance General Purpose each data and log file is a remote disk whose IOPS and throughput come from the file's size (up to 128 GB about 500 IOPS, up to 512 GB about 2,300, up to 1 TB 5,000): growing a busy file past the next size step raises its limit, and the next-gen General Purpose tier sets IOPS apart from size. Business Critical uses local SSDs. Compare each file's latency in sys.dm_io_virtual_file_stats with its size, and cut the reads too (indexes, fewer full scans). A since-startup average hides bursts: sample Waits → Right now during the busy hour."
        }
        _ => {
            "Check the volume's limits first: cloud disks throttle at their provisioned IOPS and throughput, and burst credits run out. Then look for other load on the same disk and antivirus scanning data files, and give the log its own low-latency volume. A since-startup average hides bursts: sample Waits → Right now during the busy hour."
        }
    }
}

pub fn slow_storage(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(p) = snapshot.pressure.as_ref() else { return Vec::new() };
    let files = culprit_core::signals::merge_logs(&p.files);
    slow_groups(snapshot, &files)
        .into_iter()
        .map(|g| {
            let (avg, ops, write) = (g.avg_ms(), g.ops, g.write);
            let label = file_label(snapshot, &g);
            let operation = if write && g.files.iter().all(|f| f.write_label() == "flush") { "flush" } else { "write" };
            let what = match g.files.as_slice() {
                [one] => one.name.clone(),
                _ => format!("{} files", match g.kind {
                    FileKind::Temp => "temp",
                    FileKind::Log => "log",
                    _ => "data",
                }),
            };
            let coverage = g.files.iter().map(|f| f.coverage(snapshot)).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().join("; ");
            let key = format!("{}:{what}", g.database.as_deref().unwrap_or(""));
            let mut f = finding("slow-storage", &key, Category::Configuration, subject(SubjectKind::Database, &label));
            // Only reads come in bulk sizes that slow each one down; a log is judged on its commits.
            let bulk = !write && g.bulk();
            f.title = match (write, bulk) {
                (true, _) => format!("Recorded log {} to {label} averaged {}", if operation == "flush" { "flushes" } else { "writes" }, format::duration(avg)),
                (false, false) => format!("Recorded reads from {label} averaged {}", format::duration(avg)),
                (false, true) => format!(
                    "Recorded reads from {label} averaged {}, mostly large reads of {}",
                    format::duration(avg),
                    format::bytes(g.avg_bytes().unwrap_or(0.0))
                ),
            };
            let cache_note = match snapshot.server.engine {
                Engine::Postgres => " PostgreSQL's numbers include reads the operating system served from its cache, so reads that reached the disk were slower still.",
                Engine::Mysql => " The timings include reads the operating system served from its cache unless InnoDB uses O_DIRECT.",
                Engine::Mssql | Engine::Sqlite => "",
            };
            // The label starts with a database name: never capitalize it, lead with the operation instead.
            f.summary = format!(
                "{} {} took {} on average across {} {} {}. Healthy SSD storage answers in a few milliseconds{}; when it happens during a query it can delay reads that miss memory{}. This historical average does not establish a delay now.{}",
                if write && operation == "flush" { "Flushes to" } else if write { "Writes to" } else { "Reads from" },
                label,
                format::duration(avg),
                format::count(ops),
                if write && operation == "flush" { "flushes" } else if write { "writes" } else { "reads" },
                if snapshot.server.engine == Engine::Postgres { format!("({coverage})") } else { since_start(snapshot) },
                if write && operation == "flush" { " and log flushes in under 2 ms" } else if write { " and log writes in under 2 ms" } else { "" },
                if write { " and every commit" } else { "" },
                cache_note
            );
            if bulk {
                f.summary.push_str(&format!(
                    " These reads average {} each: mostly large sequential reads (backups, CHECKDB, read-ahead of big scans), which take longer each than the page-sized reads queries wait on, so the average overstates what queries see.",
                    format::bytes(g.avg_bytes().unwrap_or(0.0))
                ));
            }
            let shared = snapshot.server.engine != Engine::Mssql
                || g.database.as_deref().is_none_or(|db| !db.eq_ignore_ascii_case(&snapshot.server.database));
            let amount = counter_rate_per_day(snapshot, g.ms);
            // Historical/shared averages do not establish a current incident in this database. A slow read also
            // needs an amount behind it: a thousand old reads on a mostly idle database need no urgent warning.
            f.severity = if bulk || shared || amount.is_none_or(|ms| ms < MEDIUM_MS_A_DAY) {
                Severity::Low
            } else { Severity::Medium };
            severity_score(&mut f);
            f.impact.label = format!("{} per {}", format::duration(avg), if write { operation } else { "read" });
            f.confidence = Confidence::Medium;
            f.metrics = vec![
                Metric::new(if write && operation == "flush" { "Average flush" } else if write { "Average write" } else { "Average read" }, avg, Unit::Ms),
                Metric::new(if write && operation == "flush" { "Flushes" } else if write { "Writes" } else { "Reads" }, ops, Unit::Count),
                Metric::text(
                    if g.files.len() == 1 { "File" } else { "Files" },
                    g.files.iter().map(|f| f.name.clone()).collect::<Vec<_>>().join(", "),
                ),
            ];
            if let Some(b) = g.avg_bytes().filter(|_| !write) {
                f.metrics.push(Metric::new("Average read size", b, Unit::Bytes));
            }
            f.fix = Some(Fix { summary: storage_fix(snapshot).into(), sql: None, cost: None });
            f.verify = vec![format!("Compare two readings after the change with the same source and reset period: divide the additional {} time by the additional operation count. Measure during representative load; the cumulative average includes older operations.", if write { operation } else { "read" })];
            f.caveats.push("Historical averages: a slow hour long ago still counts, and a later scan does not isolate new operations without comparing counter differences.".into());
            if shared {
                f.caveats.push("Historical file statistics do not establish that this database's queries are slow now; check the stated source scope and measure recent operations alongside query waits.".into());
            }
            f
        })
        .collect()
}
