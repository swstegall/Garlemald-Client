// garlemald-client — cross-platform launcher for FINAL FANTASY XIV 1.x private servers
// Copyright (C) 2026  Samuel Stegall
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published
// by the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Long-lived torrent session owner: download once, then seed while the
//! launcher stays open (unless the user opted out).
//!
//! [`TorrentService`] owns a private multi-thread tokio runtime and a
//! librqbit [`Session`]. Every public method is synchronous and
//! non-blocking - it only touches atomics (and briefly locks the
//! session/handle slots) before handing the real work to the runtime
//! via `spawn`, so it is safe to call from the egui update loop.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, SessionOptions,
    SessionPersistenceConfig, TorrentStats, TorrentStatsState,
};
use tokio::runtime::Runtime;

use super::endpoint::fetch_magnet;

/// How often the monitor loop refreshes the snapshot atomics from
/// librqbit's own stats.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Coarse lifecycle of the managed patch torrent.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentState {
    Idle = 0,
    Resolving = 1,
    Downloading = 2,
    Complete = 3,
    Seeding = 4,
    Stopped = 5,
    Error = 6,
}

impl TorrentState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => TorrentState::Resolving,
            2 => TorrentState::Downloading,
            3 => TorrentState::Complete,
            4 => TorrentState::Seeding,
            5 => TorrentState::Stopped,
            6 => TorrentState::Error,
            _ => TorrentState::Idle,
        }
    }

    fn as_u8(self) -> u8 {
        self as u8
    }
}

/// One-moment snapshot the UI polls.
#[derive(Debug, Clone)]
pub struct TorrentSnapshot {
    pub state: TorrentState,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub uploaded_bytes: u64,
    pub peers: u32,
    pub download_bps: u64,
    pub upload_bps: u64,
    pub error: Option<String>,
}

/// Errors raised by [`TorrentService`]'s synchronous entry points.
#[derive(Debug, thiserror::Error)]
pub enum TorrentServiceError {
    /// A download is already resolving or in flight.
    #[error("a torrent download is already in progress")]
    Busy,
}

/// Shared state between the sync public API and the async tasks it
/// spawns. Every field is either a lock-free atomic or a `Mutex` held
/// only for the length of a single pointer read/clone/write.
struct Inner {
    state: AtomicU8,
    progress_bytes: AtomicU64,
    total_bytes: AtomicU64,
    uploaded_bytes: AtomicU64,
    peers: AtomicU32,
    download_bps: AtomicU64,
    upload_bps: AtomicU64,
    error: Mutex<Option<String>>,
    seeding_enabled: AtomicBool,
    // Bumped every time a monitor loop is (re)spawned, so a superseded
    // loop (stop(), a re-toggled seeding flag) notices on its next tick
    // and exits instead of racing the new loop over the same atomics.
    generation: AtomicU64,
    session: Mutex<Option<Arc<Session>>>,
    handle: Mutex<Option<Arc<ManagedTorrent>>>,
    // The output root the session was actually created with. The payload
    // lives here even if the user re-points the storage preference later;
    // a preference change only takes effect once a fresh session starts.
    output_dir: Mutex<Option<PathBuf>>,
}

impl Inner {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(TorrentState::Idle.as_u8()),
            progress_bytes: AtomicU64::new(0),
            total_bytes: AtomicU64::new(0),
            uploaded_bytes: AtomicU64::new(0),
            peers: AtomicU32::new(0),
            download_bps: AtomicU64::new(0),
            upload_bps: AtomicU64::new(0),
            error: Mutex::new(None),
            seeding_enabled: AtomicBool::new(true),
            generation: AtomicU64::new(0),
            session: Mutex::new(None),
            handle: Mutex::new(None),
            output_dir: Mutex::new(None),
        }
    }

    /// True once `generation` has moved past the value a task was
    /// spawned with, i.e. a stop() or a newer task superseded it.
    fn is_superseded(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) != generation
    }

    /// Records a fatal message unless this task was already superseded
    /// (a cancelled resolve must not clobber the Stopped state with
    /// Error).
    fn fail_unless_superseded(&self, generation: u64, message: impl Into<String>) {
        if self.is_superseded(generation) {
            log::debug!("superseded torrent task error ignored: {}", message.into());
            return;
        }
        self.fail(message);
    }

    fn state(&self) -> TorrentState {
        TorrentState::from_u8(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, state: TorrentState) {
        self.state.store(state.as_u8(), Ordering::Release);
    }

    fn clear_error(&self) {
        *self.error.lock().expect("torrent error mutex poisoned") = None;
    }

    fn error(&self) -> Option<String> {
        self.error
            .lock()
            .expect("torrent error mutex poisoned")
            .clone()
    }

    /// Records a fatal message and moves to [`TorrentState::Error`].
    fn fail(&self, message: impl Into<String>) {
        let message = message.into();
        log::warn!("torrent service failed: {message}");
        *self.error.lock().expect("torrent error mutex poisoned") = Some(message);
        self.set_state(TorrentState::Error);
    }

    fn session(&self) -> Option<Arc<Session>> {
        self.session
            .lock()
            .expect("torrent session mutex poisoned")
            .clone()
    }

    fn set_session(&self, session: Arc<Session>) {
        *self.session.lock().expect("torrent session mutex poisoned") = Some(session);
    }

    fn handle(&self) -> Option<Arc<ManagedTorrent>> {
        self.handle
            .lock()
            .expect("torrent handle mutex poisoned")
            .clone()
    }

    fn set_handle(&self, handle: Arc<ManagedTorrent>) {
        *self.handle.lock().expect("torrent handle mutex poisoned") = Some(handle);
    }

    /// Refreshes the byte/peer/speed atomics from a fresh stats read.
    /// Peer count and speeds fall back to zero when the torrent has no
    /// live state (not yet started, or paused).
    fn apply_stats(&self, stats: &TorrentStats) {
        self.progress_bytes
            .store(stats.progress_bytes, Ordering::Release);
        self.total_bytes.store(stats.total_bytes, Ordering::Release);
        self.uploaded_bytes
            .store(stats.uploaded_bytes, Ordering::Release);

        let (peers, down_bps, up_bps) = match &stats.live {
            Some(live) => (
                live.snapshot.peer_stats.live as u32,
                mib_per_sec_to_bytes_per_sec(live.download_speed.mbps),
                mib_per_sec_to_bytes_per_sec(live.upload_speed.mbps),
            ),
            None => (0, 0, 0),
        };
        self.peers.store(peers, Ordering::Release);
        self.download_bps.store(down_bps, Ordering::Release);
        self.upload_bps.store(up_bps, Ordering::Release);
    }

    fn snapshot(&self) -> TorrentSnapshot {
        TorrentSnapshot {
            state: self.state(),
            progress_bytes: self.progress_bytes.load(Ordering::Acquire),
            total_bytes: self.total_bytes.load(Ordering::Acquire),
            uploaded_bytes: self.uploaded_bytes.load(Ordering::Acquire),
            peers: self.peers.load(Ordering::Acquire),
            download_bps: self.download_bps.load(Ordering::Acquire),
            upload_bps: self.upload_bps.load(Ordering::Acquire),
            error: self.error(),
        }
    }
}

/// librqbit reports transfer speed as MiB/s; the snapshot wants raw
/// bytes/sec to match the byte counters.
fn mib_per_sec_to_bytes_per_sec(mebibytes_per_second: f64) -> u64 {
    (mebibytes_per_second.max(0.0) * 1024.0 * 1024.0) as u64
}

/// Owner of the tokio runtime + librqbit session.
pub struct TorrentService {
    inner: Arc<Inner>,
    runtime: Runtime,
    persistence_dir: PathBuf,
}

impl TorrentService {
    pub fn new(persistence_dir: PathBuf) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("garlemald-torrent")
            .enable_all()
            .build()
            .expect("could not start the torrent runtime");
        Self {
            inner: Arc::new(Inner::new()),
            runtime,
            persistence_dir,
        }
    }

    /// Fetch the magnet link and start (or resume) the torrented patch
    /// download. Rejects with [`TorrentServiceError::Busy`] while a
    /// previous call is still resolving or downloading; a session that
    /// finished, stopped, or errored can be restarted.
    pub fn start_download(
        &self,
        endpoint_url: String,
        output_dir: PathBuf,
    ) -> Result<(), TorrentServiceError> {
        if matches!(
            self.inner.state(),
            TorrentState::Resolving | TorrentState::Downloading
        ) {
            return Err(TorrentServiceError::Busy);
        }

        self.inner.clear_error();
        self.inner.set_state(TorrentState::Resolving);
        let generation = self.next_generation();
        let inner = self.inner.clone();
        let persistence_dir = self.persistence_dir.clone();
        log::info!(
            "starting torrent download: endpoint={endpoint_url} output={}",
            output_dir.display()
        );
        self.runtime.spawn(async move {
            run_download(inner, generation, endpoint_url, output_dir, persistence_dir).await;
        });
        Ok(())
    }

    /// Called at app boot when the user has seeding enabled: if
    /// librqbit persisted a torrent from a previous run, resume
    /// monitoring (and seeding) it without re-fetching the magnet
    /// link. A no-op once a session already exists.
    pub fn ensure_seeding_if_complete(&self, output_dir: PathBuf) {
        if self.inner.session().is_some() {
            return;
        }
        let generation = self.next_generation();
        let inner = self.inner.clone();
        let persistence_dir = self.persistence_dir.clone();
        log::info!(
            "checking for a persisted torrent session: output={}",
            output_dir.display()
        );
        self.runtime.spawn(async move {
            resume_persisted_session(inner, generation, output_dir, persistence_dir).await;
        });
    }

    /// Flips the opt-out flag. The completion handler in the monitor
    /// loop consults it the first time the torrent finishes; this also
    /// drives an immediate pause/unpause when the torrent has already
    /// finished.
    pub fn set_seeding_enabled(&self, enabled: bool) {
        self.inner.seeding_enabled.store(enabled, Ordering::Release);

        match (self.inner.state(), enabled) {
            (TorrentState::Complete, true) => self.resume_seeding(),
            (TorrentState::Seeding, false) => self.pause_seeding(),
            _ => {}
        }
    }

    /// User cancel: pause the torrent (files and session persistence
    /// survive so a later [`TorrentService::start_download`] resumes)
    /// and move to [`TorrentState::Stopped`]. The pause task is spawned
    /// even mid-resolve: whichever of this task and the in-flight
    /// `run_download` observes the other's write pauses the torrent, so
    /// a cancel during the resolve window cannot leave it live.
    pub fn stop(&self) {
        self.invalidate_generation();
        self.inner.set_state(TorrentState::Stopped);
        log::info!("torrent download stopped by the user");

        let inner = self.inner.clone();
        self.runtime.spawn(async move {
            if let (Some(session), Some(handle)) = (inner.session(), inner.handle())
                && let Err(e) = session.pause(&handle).await
            {
                log::warn!("could not pause the torrent on stop: {e}");
            }
        });
    }

    /// The output root of the live session, when one exists. This is
    /// where the payload actually lives, which can differ from the
    /// current storage preference until the next restart.
    pub fn session_output_dir(&self) -> Option<PathBuf> {
        self.inner
            .output_dir
            .lock()
            .expect("torrent output dir mutex poisoned")
            .clone()
    }

    /// The torrent's own content folder: the output root joined with
    /// the torrent's top-level directory. Payload scans want this, not
    /// the whole storage root, which can be as large as the user's
    /// entire Documents folder.
    pub fn payload_dir(&self) -> Option<PathBuf> {
        let out = self.session_output_dir()?;
        let handle = self.inner.handle()?;
        let first = handle
            .with_metadata(|m| {
                m.file_infos.first().and_then(|fi| {
                    fi.relative_filename
                        .components()
                        .next()
                        .map(|c| c.as_os_str().to_owned())
                })
            })
            .ok()??;
        Some(out.join(first))
    }

    pub fn snapshot(&self) -> TorrentSnapshot {
        self.inner.snapshot()
    }

    /// Bumps the generation counter and returns the new value, marking
    /// the start of a fresh monitor loop.
    fn next_generation(&self) -> u64 {
        self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Bumps the generation counter without caring about the new
    /// value, invalidating whatever monitor loop is currently running.
    fn invalidate_generation(&self) {
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn resume_seeding(&self) {
        let generation = self.next_generation();
        let inner = self.inner.clone();
        self.runtime.spawn(async move {
            let (Some(session), Some(handle)) = (inner.session(), inner.handle()) else {
                return;
            };
            if let Err(e) = session.unpause(&handle).await {
                inner.fail(format!("could not resume seeding: {e}"));
                return;
            }
            inner.set_state(TorrentState::Seeding);
            run_monitor(inner, session, handle, generation, true).await;
        });
    }

    fn pause_seeding(&self) {
        self.invalidate_generation();
        let inner = self.inner.clone();
        self.runtime.spawn(async move {
            if let (Some(session), Some(handle)) = (inner.session(), inner.handle())
                && let Err(e) = session.pause(&handle).await
            {
                inner.fail(format!("could not pause seeding: {e}"));
                return;
            }
            inner.set_state(TorrentState::Complete);
        });
    }
}

/// Reuses an already-created session, or builds one rooted at
/// `output_dir` with fastresume state under `persistence_dir`.
async fn get_or_create_session(
    inner: &Inner,
    output_dir: PathBuf,
    persistence_dir: PathBuf,
) -> Result<Arc<Session>, String> {
    if let Some(session) = inner.session() {
        return Ok(session);
    }
    let opts = SessionOptions {
        fastresume: true,
        persistence: Some(SessionPersistenceConfig::Json {
            folder: Some(persistence_dir),
        }),
        ..Default::default()
    };
    let session = Session::new_with_opts(output_dir.clone(), opts)
        .await
        .map_err(|e| e.to_string())?;
    inner.set_session(session.clone());
    *inner
        .output_dir
        .lock()
        .expect("torrent output dir mutex poisoned") = Some(output_dir);
    Ok(session)
}

/// Narrows an already-managed torrent to its zip payload(s) when it
/// mixes zips with other files. Archive.org torrents in particular
/// bundle a `<item>_files.xml` that the server regenerates after the
/// torrent is created, so no peer can ever supply piece-valid data for
/// it and a full-selection download stalls at 99.9% forever, never
/// reaching `finished`. librqbit computes `finished`/`progress`/`total`
/// over the SELECTED files only, so deselecting the stragglers both
/// unblocks completion and snaps the progress readout to the payload.
/// No-op when the torrent has no zips or is zip-only. Used on the
/// session-restore path, where the add options are out of our hands;
/// fresh adds narrow via [`add_zip_narrowed`] instead.
async fn select_zip_payload_only(
    inner: &Inner,
    session: &Arc<Session>,
    handle: &Arc<ManagedTorrent>,
    generation: u64,
) {
    // update_only_files is rejected while the torrent runs its initial
    // checksum pass - the very state a just-restored torrent is in.
    // Wait it out (a full 5.7 GiB re-check can take a while).
    while matches!(handle.stats().state, TorrentStatsState::Initializing) {
        if inner.is_superseded(generation) {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    if inner.is_superseded(generation) {
        return;
    }

    let indices = handle.with_metadata(|m| {
        let zips: HashSet<usize> = m
            .file_infos
            .iter()
            .enumerate()
            .filter(|(_, fi)| {
                fi.relative_filename
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
            })
            .map(|(idx, _)| idx)
            .collect();
        (zips, m.file_infos.len())
    });
    let (zips, total) = match indices {
        Ok(pair) => pair,
        Err(e) => {
            log::warn!("could not inspect the torrent's file list: {e}");
            return;
        }
    };
    if zips.is_empty() || zips.len() == total {
        return;
    }
    log::info!(
        "narrowing the torrent to its zip payload: selected={} total={total}",
        zips.len()
    );
    if let Err(e) = session.update_only_files(handle, &zips).await {
        log::warn!("could not narrow the torrent to the zip payload: {e}");
    }
}

/// The fetch-magnet -> create-session -> add-torrent -> monitor
/// pipeline behind [`TorrentService::start_download`].
async fn run_download(
    inner: Arc<Inner>,
    generation: u64,
    endpoint_url: String,
    output_dir: PathBuf,
    persistence_dir: PathBuf,
) {
    // fetch_magnet is a sync (ureq) call; run it on the blocking pool so
    // it does not stall this runtime's worker threads.
    let magnet = match tokio::task::spawn_blocking(move || fetch_magnet(&endpoint_url)).await {
        Ok(Ok(magnet)) => magnet,
        Ok(Err(e)) => {
            inner.fail_unless_superseded(
                generation,
                format!("could not fetch the patch magnet link: {e}"),
            );
            return;
        }
        Err(e) => {
            inner.fail_unless_superseded(generation, format!("magnet fetch task failed: {e}"));
            return;
        }
    };

    let session = match get_or_create_session(&inner, output_dir, persistence_dir).await {
        Ok(session) => session,
        Err(e) => {
            inner.fail_unless_superseded(
                generation,
                format!("could not start the torrent session: {e}"),
            );
            return;
        }
    };

    let handle = match add_zip_narrowed(&session, magnet.as_str()).await {
        Ok(handle) => handle,
        Err(e) => {
            inner.fail_unless_superseded(generation, e);
            return;
        }
    };

    inner.set_handle(handle.clone());
    if inner.is_superseded(generation) {
        // stop() won the race mid-resolve: the torrent was added anyway,
        // so pause it here instead of letting it download unobserved.
        if let Err(e) = session.pause(&handle).await {
            log::warn!("could not pause a torrent added after stop: {e}");
        }
        return;
    }
    // A download is (re)starting; the completion stamp no longer holds.
    // The monitor rewrites it the moment the torrent reports finished.
    clear_complete_marker(&inner);
    inner.set_state(TorrentState::Downloading);
    log::info!("patch torrent added; downloading");
    run_monitor(inner, session, handle, generation, false).await;
}

/// Filename pattern of the patch payload inside the torrent.
const ZIP_ONLY_REGEX: &str = r"(?i)\.zip$";

/// Our own "the payload finished downloading" stamp, written into the
/// output root when a download completes and removed when a new one
/// starts. librqbit's fastresume has been observed failing validation
/// on every restart and then re-checking a fully downloaded payload as
/// empty in milliseconds - which, left alone, silently re-downloads
/// 5.7 GiB over the good file. The boot path trusts this marker over
/// that verdict and refuses to let a restore clobber a finished
/// payload.
const COMPLETE_MARKER: &str = ".garlemald-patches-complete";

fn complete_marker_path(output_dir: &Path) -> PathBuf {
    output_dir.join(COMPLETE_MARKER)
}

fn write_complete_marker(inner: &Inner) {
    let Some(dir) = inner
        .output_dir
        .lock()
        .expect("torrent output dir mutex poisoned")
        .clone()
    else {
        return;
    };
    if let Err(e) = std::fs::write(complete_marker_path(&dir), b"") {
        log::warn!("could not write the download-complete marker: {e}");
    }
}

fn clear_complete_marker(inner: &Inner) {
    let Some(dir) = inner
        .output_dir
        .lock()
        .expect("torrent output dir mutex poisoned")
        .clone()
    else {
        return;
    };
    match std::fs::remove_file(complete_marker_path(&dir)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("could not clear the download-complete marker: {e}"),
    }
}

fn complete_marker_exists(inner: &Inner) -> bool {
    inner
        .output_dir
        .lock()
        .expect("torrent output dir mutex poisoned")
        .as_deref()
        .is_some_and(|dir| complete_marker_path(dir).is_file())
}

/// Adds the patch torrent with the selection narrowed to its zip
/// payload. Narrowing must ride in the add options: `update_only_files`
/// is rejected while a torrent is initializing, which is exactly the
/// state a torrent is in right after an add or a session restore. An
/// already-managed torrent kept whatever selection (and initializer
/// state) it was restored with, so it is deleted - files stay on disk -
/// and re-added, which both applies the narrowed selection and forces a
/// clean re-check of the existing data. Falls back to the full torrent
/// when nothing matches the zip regex (a future raw-patch torrent).
async fn add_zip_narrowed(
    session: &Arc<Session>,
    magnet: &str,
) -> Result<Arc<ManagedTorrent>, String> {
    let mut narrowed = true;
    // At most: narrowed add -> delete already-managed -> narrowed re-add
    // -> wide retry. Anything longer means something is genuinely wrong.
    for _ in 0..4 {
        let opts = AddTorrentOptions {
            overwrite: true,
            only_files_regex: narrowed.then(|| ZIP_ONLY_REGEX.to_string()),
            ..Default::default()
        };
        let response = match session
            .add_torrent(AddTorrent::from_url(magnet), Some(opts))
            .await
        {
            Ok(response) => response,
            Err(e) if narrowed => {
                // Typically "no files matched the regex"; retry unfiltered
                // so a torrent that ships loose .patch files still works.
                log::info!("zip-narrowed add failed; retrying with the full torrent: {e}");
                narrowed = false;
                continue;
            }
            Err(e) => return Err(format!("could not add the patch torrent: {e}")),
        };
        match response {
            AddTorrentResponse::Added(_, handle) => return Ok(handle),
            AddTorrentResponse::AlreadyManaged(id, _) => {
                log::info!("dropping the previously managed torrent to re-add it zip-narrowed");
                session
                    .delete(id.into(), false)
                    .await
                    .map_err(|e| format!("could not drop the stale managed torrent: {e}"))?;
            }
            AddTorrentResponse::ListOnly(_) => {
                return Err("torrent session returned an unexpected list-only response".into());
            }
        }
    }
    Err("could not add the patch torrent after several attempts".into())
}

/// Attaches to whatever librqbit's persistence restored on startup, if
/// anything. Leaves the state at [`TorrentState::Idle`] (the default)
/// when nothing was persisted.
async fn resume_persisted_session(
    inner: Arc<Inner>,
    generation: u64,
    output_dir: PathBuf,
    persistence_dir: PathBuf,
) {
    // Nothing was ever persisted: skip building a session at all. A
    // launcher that never torrented then opens no BitTorrent networking
    // at boot, and - because get_or_create_session pins the output root
    // of whichever session exists first - a storage-preference change
    // made before the first download still takes effect instead of
    // being silently overridden by a boot-created empty session.
    let has_state = std::fs::read_dir(&persistence_dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| !e.file_name().to_string_lossy().starts_with('.'))
        })
        .unwrap_or(false);
    if !has_state {
        log::info!("no persisted torrent session to resume");
        return;
    }

    let session = match get_or_create_session(&inner, output_dir, persistence_dir).await {
        Ok(session) => session,
        Err(e) => {
            inner.fail(format!("could not restore the torrent session: {e}"));
            return;
        }
    };

    let restored =
        session.with_torrents(|torrents| torrents.next().map(|(_, handle)| handle.clone()));
    let Some(handle) = restored else {
        log::info!("no persisted torrent to resume");
        return;
    };

    log::info!("resuming a persisted torrent session");
    inner.set_handle(handle.clone());
    inner.set_state(TorrentState::Downloading);

    // The restore may still be running its initial checksum pass. An
    // unpause during that window spawns a SECOND initializer that races
    // the first over the same file handles - observed corrupting the
    // check into "have 0" / "file is None" and invalidating perfectly
    // good on-disk data. Wait for it to settle first, and only wake the
    // torrent if it actually restored paused.
    while matches!(handle.stats().state, TorrentStatsState::Initializing) {
        if inner.is_superseded(generation) {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    if inner.is_superseded(generation) {
        return;
    }
    // Restore-check verdict vs our own completion stamp: when the
    // marker says the payload finished but the restore checked it as
    // incomplete, the restore is the party we distrust (its fastresume
    // has been observed re-checking a finished 5.7 GiB payload as empty
    // in milliseconds). Downloading here would clobber a good payload
    // mid-apply, so hold the torrent paused instead; a fresh Download
    // click heals seeding via the clean delete + re-add path.
    if !handle.stats().finished && complete_marker_exists(&inner) {
        log::warn!(
            "restore reports the payload incomplete but the completion marker exists; \
             holding the torrent paused instead of re-downloading"
        );
        if let Err(e) = session.pause(&handle).await {
            log::debug!("restored torrent was already paused: {e}");
        }
        inner.set_state(TorrentState::Complete);
        return;
    }
    if matches!(handle.stats().state, TorrentStatsState::Paused) {
        // This path only runs when the user has seeding on.
        if let Err(e) = session.unpause(&handle).await {
            log::warn!("could not wake the restored torrent: {e}");
        }
    }
    // Heals sessions persisted before zip-only selection existed; waits
    // out any (post-unpause) checksum pass internally.
    select_zip_payload_only(&inner, &session, &handle, generation).await;
    if inner.is_superseded(generation) {
        return;
    }
    run_monitor(inner, session, handle, generation, false).await;
}

/// Polls `handle.stats()` every [`POLL_INTERVAL`], refreshing the
/// snapshot atomics. `already_seeding` skips straight to the
/// keep-polling-forever branch (used when resuming from
/// [`TorrentState::Complete`]); otherwise the loop watches for the
/// torrent's first completion and makes the one-time seed-or-pause
/// decision itself. After that point, toggling seeding is owned by
/// [`TorrentService::set_seeding_enabled`] - the loop only refreshes
/// counters.
async fn run_monitor(
    inner: Arc<Inner>,
    session: Arc<Session>,
    handle: Arc<ManagedTorrent>,
    generation: u64,
    already_seeding: bool,
) {
    let mut past_completion = already_seeding;
    // Throttles the info-level progress line below to roughly once every 10s
    // (POLL_INTERVAL is 500ms) instead of spamming it every tick.
    let mut tick: u32 = 0;
    loop {
        if inner.generation.load(Ordering::Acquire) != generation {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        if inner.generation.load(Ordering::Acquire) != generation {
            return;
        }

        let stats = handle.stats();
        inner.apply_stats(&stats);

        // A torrent that librqbit itself moved to Error (a wedged
        // initializer, unwritable storage) would otherwise look like a
        // silently stalled download forever.
        if matches!(stats.state, TorrentStatsState::Error) {
            inner.fail(
                stats
                    .error
                    .unwrap_or_else(|| "torrent entered an error state".to_string()),
            );
            return;
        }

        if past_completion {
            continue;
        }

        if !stats.finished {
            inner.set_state(TorrentState::Downloading);
            tick = tick.wrapping_add(1);
            if tick.is_multiple_of(20) {
                let total = inner.total_bytes.load(Ordering::Acquire);
                let progress = inner.progress_bytes.load(Ordering::Acquire);
                let percent = if total > 0 {
                    (progress as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                log::info!(
                    "torrent download progress: {progress}/{total} bytes ({percent:.1}%), {} peers, {} B/s down",
                    inner.peers.load(Ordering::Acquire),
                    inner.download_bps.load(Ordering::Acquire)
                );
            }
            continue;
        }

        past_completion = true;
        write_complete_marker(&inner);
        if inner.seeding_enabled.load(Ordering::Acquire) {
            inner.set_state(TorrentState::Seeding);
            log::info!("patch torrent finished; seeding");
            continue;
        }

        if let Err(e) = session.pause(&handle).await {
            inner.fail(format!("could not pause the completed torrent: {e}"));
            return;
        }
        inner.set_state(TorrentState::Complete);
        log::info!("patch torrent finished; seeding disabled");
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torrent_state_round_trips_through_u8() {
        for state in [
            TorrentState::Idle,
            TorrentState::Resolving,
            TorrentState::Downloading,
            TorrentState::Complete,
            TorrentState::Seeding,
            TorrentState::Stopped,
            TorrentState::Error,
        ] {
            assert_eq!(TorrentState::from_u8(state.as_u8()), state);
        }
    }

    #[test]
    fn new_service_snapshot_is_idle_with_zeroed_counters() {
        let dir = tempfile::tempdir().unwrap();
        let service = TorrentService::new(dir.path().to_path_buf());
        let snapshot = service.snapshot();
        assert_eq!(snapshot.state, TorrentState::Idle);
        assert_eq!(snapshot.progress_bytes, 0);
        assert_eq!(snapshot.total_bytes, 0);
        assert_eq!(snapshot.uploaded_bytes, 0);
        assert_eq!(snapshot.peers, 0);
        assert_eq!(snapshot.download_bps, 0);
        assert_eq!(snapshot.upload_bps, 0);
        assert_eq!(snapshot.error, None);
    }

    #[test]
    fn start_download_twice_returns_busy_the_second_time() {
        let dir = tempfile::tempdir().unwrap();
        let service = TorrentService::new(dir.path().to_path_buf());
        // Not a valid URL: fetch_magnet fails synchronously inside the
        // spawned task with no real network I/O, so this never races
        // the busy check below.
        let first = service.start_download("not-a-valid-url".to_string(), dir.path().to_path_buf());
        assert!(first.is_ok());

        let second =
            service.start_download("not-a-valid-url".to_string(), dir.path().to_path_buf());
        assert!(
            matches!(second, Err(TorrentServiceError::Busy)),
            "got {second:?}"
        );
    }

    #[test]
    fn set_seeding_enabled_is_inert_without_an_active_session() {
        let dir = tempfile::tempdir().unwrap();
        let service = TorrentService::new(dir.path().to_path_buf());

        service.set_seeding_enabled(false);
        assert_eq!(service.snapshot().state, TorrentState::Idle);

        service.set_seeding_enabled(true);
        assert_eq!(service.snapshot().state, TorrentState::Idle);
    }
}
