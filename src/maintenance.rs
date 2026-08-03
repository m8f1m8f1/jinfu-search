use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::watch;

use crate::{AutoVolumeState, IndexedRoot, SearchIndex, local_fixed_volumes};

const USN_SYNC_INTERVAL: Duration = Duration::from_secs(15);
const FULL_RECONCILE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const EVENT_DEBOUNCE: Duration = Duration::from_millis(750);
const MAX_PENDING_PATHS: usize = 20_000;
const EVENT_QUEUE_CAPACITY: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolumeAction {
    FullSnapshot(char),
    SyncUsn(char),
    WatchOnly(char),
}

/// 启动唯一的低优先级维护线程。它串行执行 MFT 快照、USN 增量与文件事件合并，
/// 不占用 GUI、Named Pipe 或 MCP 的工作线程。
pub fn start(index: SearchIndex, shutdown: watch::Receiver<bool>) {
    let _ = thread::Builder::new()
        .name("jinfu-search-maintenance".to_owned())
        .spawn(move || run(index, shutdown));
}

fn run(index: SearchIndex, shutdown: watch::Receiver<bool>) {
    let _background_mode = BackgroundThreadMode::begin();
    // 首次快照可能持续数分钟。使用有界队列，避免这段时间的文件系统事件无限
    // 堆积；一旦溢出就记为“需要完整校准”，而不是悄悄丢事件。
    let (events_tx, events_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
    let events_overflowed = Arc::new(AtomicBool::new(false));
    let callback_overflowed = events_overflowed.clone();
    let mut watcher = notify::recommended_watcher(move |event| {
        if let Err(TrySendError::Full(_)) = events_tx.try_send(event) {
            callback_overflowed.store(true, Ordering::Release);
        }
    })
    .ok();

    if let Some(watcher) = watcher.as_mut() {
        for volume in local_fixed_volumes() {
            let _ = watcher.watch(
                &PathBuf::from(format!("{volume}:\\")),
                RecursiveMode::Recursive,
            );
        }
    }

    reconcile(&index, false);
    let mut next_usn_sync = Instant::now() + USN_SYNC_INTERVAL;
    let mut next_full_reconcile = Instant::now() + FULL_RECONCILE_INTERVAL;

    while !shutdown_requested(&shutdown) {
        if events_overflowed.swap(false, Ordering::AcqRel) {
            while events_rx.try_recv().is_ok() {}
            reconcile(&index, true);
        }
        let now = Instant::now();
        if now >= next_usn_sync {
            let _ = index.sync_indexed_ntfs_volumes();
            next_usn_sync = Instant::now() + USN_SYNC_INTERVAL;
        }
        if now >= next_full_reconcile {
            reconcile(&index, false);
            next_full_reconcile = Instant::now() + FULL_RECONCILE_INTERVAL;
        }

        let wait = next_usn_sync
            .min(next_full_reconcile)
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(1));
        match events_rx.recv_timeout(wait) {
            Ok(Ok(event)) => process_event_batch(&index, &events_rx, event),
            Ok(Err(_)) => reconcile(&index, true),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn process_event_batch(
    index: &SearchIndex,
    receiver: &mpsc::Receiver<notify::Result<Event>>,
    first: Event,
) {
    let mut paths = HashSet::new();
    let mut force_reconcile = collect_event(first, &mut paths);
    let deadline = Instant::now() + EVENT_DEBOUNCE;
    while paths.len() < MAX_PENDING_PATHS {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(remaining) {
            Ok(Ok(event)) => force_reconcile |= collect_event(event, &mut paths),
            Ok(Err(_)) => force_reconcile = true,
            Err(_) => break,
        }
    }
    if paths.len() >= MAX_PENDING_PATHS {
        force_reconcile = true;
    }
    if force_reconcile {
        reconcile(index, true);
        return;
    }

    let roots = match index.indexed_roots() {
        Ok(roots) => roots,
        Err(_) => return,
    };
    for root in roots.into_iter().filter(|root| !root.uses_ntfs_usn) {
        let root_path = PathBuf::from(root.root.replace('/', "\\"));
        let changed = paths
            .iter()
            .filter(|path| path.starts_with(&root_path))
            .cloned()
            .collect::<Vec<_>>();
        if !changed.is_empty() && index.refresh_changed_paths(&root_path, &changed).is_err() {
            reconcile(index, true);
            return;
        }
    }
}

fn collect_event(event: Event, paths: &mut HashSet<PathBuf>) -> bool {
    if event.need_rescan() {
        return true;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    paths.extend(event.paths);
    false
}

fn reconcile(index: &SearchIndex, force_non_usn: bool) {
    let volumes = local_fixed_volumes();
    let roots = index.indexed_roots().unwrap_or_default();
    let states = index.auto_volume_states().unwrap_or_default();
    let now_ms = unix_ms();
    for action in plan_actions(&volumes, &roots, &states, now_ms, force_non_usn) {
        match action {
            VolumeAction::FullSnapshot(volume) => {
                let result = index
                    .scan_ntfs_volume(volume)
                    .or_else(|_| index.scan_root(format!("{volume}:\\")));
                if result.is_ok() {
                    let root = format!("{volume}:/");
                    let uses_usn = index
                        .indexed_roots()
                        .unwrap_or_default()
                        .into_iter()
                        .find(|candidate| candidate.root == root)
                        .is_some_and(|candidate| candidate.uses_ntfs_usn);
                    let _ = index.record_auto_volume_state(&root, uses_usn);
                }
            }
            VolumeAction::SyncUsn(volume) => {
                let _ = index.sync_ntfs_volume(volume);
            }
            VolumeAction::WatchOnly(_) => {}
        }
    }
}

fn plan_actions(
    volumes: &[char],
    roots: &[IndexedRoot],
    states: &[AutoVolumeState],
    now_ms: i64,
    force_non_usn: bool,
) -> Vec<VolumeAction> {
    let roots = roots
        .iter()
        .map(|root| (root.root.as_str(), root))
        .collect::<HashMap<_, _>>();
    let states = states
        .iter()
        .map(|state| (state.root.as_str(), state))
        .collect::<HashMap<_, _>>();
    let stale_before = now_ms.saturating_sub(FULL_RECONCILE_INTERVAL.as_millis() as i64);

    volumes
        .iter()
        .map(|volume| {
            let volume = volume.to_ascii_uppercase();
            let root_name = format!("{volume}:/");
            let root = roots.get(root_name.as_str());
            let state = states.get(root_name.as_str());
            match (root, state) {
                (Some(root), Some(_state)) if root.uses_ntfs_usn => VolumeAction::SyncUsn(volume),
                (Some(_), Some(state))
                    if !force_non_usn
                        && !state.uses_ntfs_usn
                        && state.last_reconcile_unix_ms > stale_before =>
                {
                    VolumeAction::WatchOnly(volume)
                }
                _ => VolumeAction::FullSnapshot(volume),
            }
        })
        .collect()
}

fn shutdown_requested(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

fn unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

struct BackgroundThreadMode(bool);

impl BackgroundThreadMode {
    fn begin() -> Self {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };

        let enabled =
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) != 0 };
        Self(enabled)
    }
}

impl Drop for BackgroundThreadMode {
    fn drop(&mut self) {
        if self.0 {
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END,
            };
            unsafe {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str, uses_ntfs_usn: bool) -> IndexedRoot {
        IndexedRoot {
            root: name.to_owned(),
            last_scan_unix_ms: 0,
            complete: true,
            skipped_entries: 0,
            uses_ntfs_usn,
        }
    }

    fn state(name: &str, age_ms: i64, uses_ntfs_usn: bool) -> AutoVolumeState {
        AutoVolumeState {
            root: name.to_owned(),
            last_reconcile_unix_ms: age_ms,
            uses_ntfs_usn,
        }
    }

    #[test]
    fn first_start_scans_every_fixed_volume_without_manual_input() {
        assert_eq!(
            plan_actions(&['C', 'D'], &[], &[], 1_000, false),
            vec![
                VolumeAction::FullSnapshot('C'),
                VolumeAction::FullSnapshot('D')
            ]
        );
    }

    #[test]
    fn usn_volumes_sync_while_recent_no_journal_volumes_use_events() {
        let now = FULL_RECONCILE_INTERVAL.as_millis() as i64 + 10_000;
        let actions = plan_actions(
            &['C', 'E'],
            &[root("C:/", true), root("E:/", false)],
            &[state("C:/", now, true), state("E:/", now, false)],
            now,
            false,
        );
        assert_eq!(
            actions,
            vec![VolumeAction::SyncUsn('C'), VolumeAction::WatchOnly('E')]
        );
    }

    #[test]
    fn stale_or_overflowed_no_journal_volume_gets_a_full_reconciliation() {
        let now = FULL_RECONCILE_INTERVAL.as_millis() as i64 + 10_000;
        let roots = [root("E:/", false)];
        let states = [state("E:/", 1, false)];
        assert_eq!(
            plan_actions(&['E'], &roots, &states, now, false),
            vec![VolumeAction::FullSnapshot('E')]
        );
        assert_eq!(
            plan_actions(&['E'], &roots, &[state("E:/", now, false)], now, true),
            vec![VolumeAction::FullSnapshot('E')]
        );
    }
}
