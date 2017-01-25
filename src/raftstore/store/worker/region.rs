// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.


use std::fmt::{self, Formatter, Display};
use std::error;
use std::sync::Arc;
use std::sync::mpsc::SyncSender;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use std::str;

use rocksdb::{DB, Writable, WriteBatch};
use kvproto::raft_serverpb::{RaftApplyState, RegionLocalState, PeerState};
use kvproto::eraftpb::Snapshot as RaftSnapshot;

use util::worker::Runnable;
use util::{escape, HandyRwLock, rocksdb};
use raftstore::store::engine::{Mutable, Snapshot, Iterable};
use raftstore::store::peer_storage::{JOB_STATUS_FINISHED, JOB_STATUS_CANCELLED, JOB_STATUS_FAILED,
                                     JOB_STATUS_CANCELLING, JOB_STATUS_PENDING, JOB_STATUS_RUNNING};
use raftstore::store::{self, SnapManager, SnapKey, SnapEntry, keys, Peekable};
use storage::CF_RAFT;
use rocksdb::IngestExternalFileOptions;

use super::metrics::*;

/// region related task.
pub enum Task {
    Gen {
        region_id: u64,
        notifier: SyncSender<RaftSnapshot>,
    },
    Apply {
        region_id: u64,
        status: Arc<AtomicUsize>,
    },
    /// Destroy data between [start_key, end_key).
    ///
    /// The deletion may and may not succeed.
    Destroy {
        region_id: u64,
        start_key: Vec<u8>,
        end_key: Vec<u8>,
    },
}

impl Task {
    pub fn destroy(region_id: u64, start_key: Vec<u8>, end_key: Vec<u8>) -> Task {
        Task::Destroy {
            region_id: region_id,
            start_key: start_key,
            end_key: end_key,
        }
    }
}

impl Display for Task {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            Task::Gen { region_id, .. } => write!(f, "Snap gen for {}", region_id),
            Task::Apply { region_id, .. } => write!(f, "Snap apply for {}", region_id),
            Task::Destroy { region_id, ref start_key, ref end_key } => {
                write!(f,
                       "Destroy {} [{}, {})",
                       region_id,
                       escape(&start_key),
                       escape(&end_key))
            }
        }
    }
}

quick_error! {
    #[derive(Debug)]
    enum Error {
        Abort {
            description("abort")
            display("abort")
        }
        Other(err: Box<error::Error + Sync + Send>) {
            from()
            cause(err.as_ref())
            description(err.description())
            display("snap failed {:?}", err)
        }
    }
}

#[inline]
fn check_abort(status: &AtomicUsize) -> Result<(), Error> {
    if status.load(Ordering::Relaxed) == JOB_STATUS_CANCELLING {
        return Err(Error::Abort);
    }
    Ok(())
}

// TODO: use threadpool to do task concurrently
pub struct Runner {
    db: Arc<DB>,
    batch_size: usize,
    mgr: SnapManager,
}

impl Runner {
    pub fn new(db: Arc<DB>, mgr: SnapManager, batch_size: usize) -> Runner {
        Runner {
            db: db,
            mgr: mgr,
            batch_size: batch_size,
        }
    }

    fn generate_snap(&self,
                     region_id: u64,
                     notifier: SyncSender<RaftSnapshot>)
                     -> Result<(), Error> {
        // do we need to check leader here?
        let raw_snap = Snapshot::new(self.db.clone());

        let snap = box_try!(store::do_snapshot(self.mgr.clone(), &raw_snap, region_id));
        if let Err(e) = notifier.try_send(snap) {
            info!("[region {}] failed to notify snap result, maybe leadership has changed, \
                   ignore: {:?}",
                  region_id,
                  e);
        }
        Ok(())
    }

    fn handle_gen(&self, region_id: u64, notifier: SyncSender<RaftSnapshot>) {
        SNAP_COUNTER_VEC.with_label_values(&["generate", "all"]).inc();
        let gen_histogram = SNAP_HISTOGRAM.with_label_values(&["generate"]);
        let timer = gen_histogram.start_timer();

        if let Err(e) = self.generate_snap(region_id, notifier) {
            error!("[region {}] failed to generate snap: {:?}!!!", region_id, e);
            return;
        }

        SNAP_COUNTER_VEC.with_label_values(&["generate", "success"]).inc();
        timer.observe_duration();
    }

    fn delete_all_in_range(&self,
                           start_key: &[u8],
                           end_key: &[u8],
                           abort: &AtomicUsize)
                           -> Result<(), Error> {
        let mut wb = WriteBatch::new();
        let mut size_cnt = 0;
        for cf in self.db.cf_names() {
            try!(check_abort(&abort));
            let handle = box_try!(rocksdb::get_cf_handle(&self.db, cf));

            let mut it = box_try!(self.db.new_iterator_cf(cf, Some(end_key), false));

            try!(check_abort(&abort));
            it.seek(start_key.into());
            while it.valid() {
                {
                    let key = it.key();
                    if key >= end_key {
                        break;
                    }

                    box_try!(wb.delete_cf(handle, key));
                    size_cnt += key.len();
                    if size_cnt >= self.batch_size {
                        // Can't use write_without_wal here.
                        // Otherwise it may cause dirty data when applying snapshot.
                        box_try!(self.db.write(wb));
                        wb = WriteBatch::new();
                        size_cnt = 0;
                    }
                };
                try!(check_abort(&abort));
                if !it.next() {
                    break;
                }
            }
        }

        if wb.count() > 0 {
            box_try!(self.db.write(wb));
        }
        Ok(())
    }

    fn apply_snap(&self, region_id: u64, abort: Arc<AtomicUsize>) -> Result<(), Error> {
        info!("[region {}] begin apply snap data", region_id);
        try!(check_abort(&abort));
        let region_key = keys::region_state_key(region_id);
        let mut region_state: RegionLocalState = match box_try!(self.db.get_msg(&region_key)) {
            Some(state) => state,
            None => return Err(box_err!("failed to get region_state from {}", escape(&region_key))),
        };

        // clear up origin data.
        let region = region_state.get_region().clone();
        let start_key = keys::enc_start_key(&region);
        let end_key = keys::enc_end_key(&region);
        box_try!(self.delete_all_in_range(&start_key, &end_key, &abort));

        let state_key = keys::apply_state_key(region_id);
        let apply_state: RaftApplyState = match box_try!(self.db.get_msg_cf(CF_RAFT, &state_key)) {
            Some(state) => state,
            None => return Err(box_err!("failed to get raftstate from {}", escape(&state_key))),
        };
        let term = apply_state.get_truncated_state().get_term();
        let idx = apply_state.get_truncated_state().get_index();
        let snap_key = SnapKey::new(region_id, term, idx);
        let snap_reader = box_try!(self.mgr.rl().get_recv_snap_file_reader(&snap_key));
        self.mgr.wl().register(snap_key.clone(), SnapEntry::Applying);
        defer!({
            self.mgr.wl().deregister(&snap_key, &SnapEntry::Applying);
        });
        try!(check_abort(&abort));

        let timer = Instant::now();
        // Write the snapshot into the region.
        let mut snapshot_size = 0;
        for (cf, fpath, size) in snap_reader.list_cf_files() {
            // TODO: avoid too many allocation
            try!(check_abort(&abort));

            let cf_handle = box_try!(rocksdb::get_cf_handle(&self.db, &cf));
            let ingest_opt = IngestExternalFileOptions::new().move_files(true);
            if let Err(e) = self.db.ingest_external_file_cf(cf_handle, &ingest_opt, &[&fpath]) {
                return Err(Error::Other(box_err!(e)));
            }
            snapshot_size += size;
        }
        snap_reader.delete();
        SNAPSHOT_SIZE_HISTOGRAM.observe(snapshot_size as f64);

        region_state.set_state(PeerState::Normal);
        box_try!(self.db.put_msg(&region_key, &region_state));
        info!("[region {}] apply new data takes {:?}",
              region_id,
              timer.elapsed());
        Ok(())
    }

    fn handle_apply(&self, region_id: u64, status: Arc<AtomicUsize>) {
        status.compare_and_swap(JOB_STATUS_PENDING, JOB_STATUS_RUNNING, Ordering::SeqCst);
        SNAP_COUNTER_VEC.with_label_values(&["apply", "all"]).inc();
        let apply_histogram = SNAP_HISTOGRAM.with_label_values(&["apply"]);
        let timer = apply_histogram.start_timer();

        match self.apply_snap(region_id, status.clone()) {
            Ok(()) => {
                status.swap(JOB_STATUS_FINISHED, Ordering::SeqCst);
                SNAP_COUNTER_VEC.with_label_values(&["apply", "success"]).inc();
            }
            Err(Error::Abort) => {
                warn!("applying snapshot for region {} is aborted.", region_id);
                assert!(status.swap(JOB_STATUS_CANCELLED, Ordering::SeqCst) ==
                        JOB_STATUS_CANCELLING);
                SNAP_COUNTER_VEC.with_label_values(&["apply", "abort"]).inc();
            }
            Err(e) => {
                error!("failed to apply snap: {:?}!!!", e);
                status.swap(JOB_STATUS_FAILED, Ordering::SeqCst);
                SNAP_COUNTER_VEC.with_label_values(&["apply", "fail"]).inc();
            }
        }

        timer.observe_duration();
    }

    fn handle_destroy(&mut self, region_id: u64, start_key: Vec<u8>, end_key: Vec<u8>) {
        info!("[region {}] deleting data in [{}, {})",
              region_id,
              escape(&start_key),
              escape(&end_key));
        let status = AtomicUsize::new(JOB_STATUS_PENDING);
        if let Err(e) = self.delete_all_in_range(&start_key, &end_key, &status) {
            error!("failed to delete data in [{}, {}): {:?}",
                   escape(&start_key),
                   escape(&end_key),
                   e);
        }
    }
}

impl Runnable<Task> for Runner {
    fn run(&mut self, task: Task) {
        match task {
            Task::Gen { region_id, notifier } => self.handle_gen(region_id, notifier),
            Task::Apply { region_id, status } => self.handle_apply(region_id, status),
            Task::Destroy { region_id, start_key, end_key } => {
                self.handle_destroy(region_id, start_key, end_key)
            }
        }
    }
}
