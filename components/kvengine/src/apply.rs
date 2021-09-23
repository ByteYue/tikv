// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use crate::*;
use crate::{
    meta::is_move_down,
    table::{
        search,
        sstable::{self, SSTable},
    },
};
use crossbeam_epoch as epoch;
use kvenginepb as pb;
use slog_global::info;
use std::collections::HashSet;
use std::fs;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};

impl Engine {
    pub fn apply_change_set(&self, cs: pb::ChangeSet) -> Result<()> {
        self.pre_load_files(&cs)?;
        let g = &crossbeam_epoch::pin();
        let shard = self.get_shard(cs.shard_id, g);
        if shard.is_none() {
            return Err(Error::ShardNotFound);
        }
        let shard = shard.unwrap();
        if shard.ver != cs.shard_ver {
            return Err(Error::ShardNotMatch);
        }
        let seq = load_u64(&shard.meta_seq);
        if seq >= cs.sequence {
            warn!(
                "{}:{} skip duplicated shard seq:{}, change seq:{}",
                shard.id, shard.ver, seq, cs.sequence
            );
            return Ok(());
        } else {
            store_u64(&shard.meta_seq, cs.sequence);
        }
        if cs.has_flush() {
            self.apply_flush(shard, g, cs)?
        } else if cs.has_compaction() {
            let resut = self.apply_compaction(shard, g, cs);
            store_bool(&shard.compacting, false);
            if resut.is_err() {
                return resut;
            }
        } else if cs.has_split_files() {
            self.apply_split_files(shard, g, cs)?
        }
        shard.refresh_estimated_size();
        Ok(())
    }

    pub fn apply_flush<'a>(
        &self,
        shard: &'a Shard,
        g: &'a epoch::Guard,
        cs: pb::ChangeSet,
    ) -> Result<()> {
        let flush = cs.get_flush();
        if flush.has_l0_create() {
            let opts = dfs::Options::new(shard.id, shard.ver);
            let file = self.fs.open(flush.get_l0_create().id, opts)?;
            let l0_tbl = sstable::L0Table::new(file, self.cache.clone())?;
            shard.atomic_add_l0_table(g, l0_tbl);
            shard.atomic_remove_mem_table(g);
        }
        shard.set_split_stage(cs.stage);
        store_bool(&shard.initial_flushed, true);
        Ok(())
    }

    fn apply_compaction<'a>(
        &self,
        shard: &'a Shard,
        g: &'a epoch::Guard,
        mut cs: pb::ChangeSet,
    ) -> Result<()> {
        let comp = cs.take_compaction();
        let mut del_files = HashSet::new();
        if comp.conflicted {
            if is_move_down(&comp) {
                return Ok(());
            }
            for create in comp.get_table_creates() {
                del_files.insert(create.id);
            }
            self.remove_dfs_files(shard, g, del_files);
            return Ok(());
        }
        if comp.level == 0 {
            let l0_tbls = shard.get_l0_tbls(g);
            for tbl in &l0_tbls.tbls {
                let id = tbl.id();
                if comp.top_deletes.contains(&id) {
                    del_files.insert(id);
                }
            }
            for cf in 0..NUM_CFS {
                self.compaction_update_level_handler(
                    shard,
                    g,
                    cf,
                    1,
                    comp.get_table_creates(),
                    comp.get_bottom_deletes(),
                    &mut del_files,
                )?;
            }
            shard.atomic_remove_l0_tables(g, comp.top_deletes.len());
        } else {
            let cf = comp.cf as usize;
            self.compaction_update_level_handler(
                shard,
                g,
                cf,
                comp.level + 1,
                comp.get_table_creates(),
                comp.get_bottom_deletes(),
                &mut del_files,
            )?;
            self.compaction_update_level_handler(
                shard,
                g,
                cf,
                comp.level,
                &[],
                comp.get_top_deletes(),
                &mut del_files,
            )?;
            // For move down operation, the TableCreates may contains TopDeletes, we don't want to delete them.
            for create in comp.get_table_creates() {
                del_files.remove(&create.id);
            }
        }
        self.remove_dfs_files(shard, g, del_files);
        Ok(())
    }

    fn remove_dfs_files<'a>(&self, shard: &'a Shard, g: &'a epoch::Guard, del_files: HashSet<u64>) {
        let fs = self.fs.clone();
        let opts = dfs::Options::new(shard.id, shard.ver);
        g.defer(move || {
            for id in del_files {
                fs.remove(id, opts)
            }
        });
    }

    fn compaction_update_level_handler<'a>(
        &self,
        shard: &'a Shard,
        g: &'a epoch::Guard,
        cf: usize,
        level: u32,
        creates: &[pb::TableCreate],
        del_ids: &[u64],
        del_files: &mut HashSet<u64>,
    ) -> Result<()> {
        let opts = dfs::Options::new(shard.id, shard.ver);
        let shared = shard.cfs[cf].load(std::sync::atomic::Ordering::Acquire, g);
        let old_scf = shard.get_cf(cf, g);
        let mut new_scf = old_scf.clone();
        let level_idx = level as usize - 1;
        let mut new_level = &mut new_scf.levels[level_idx];
        let old_level = &old_scf.levels[level_idx];
        new_level.tables.truncate(0);
        new_level.total_size = 0;
        let mut need_update = false;
        for create in creates {
            if create.cf as usize != cf {
                continue;
            }
            let file = self.fs.open(create.id, opts)?;
            let tbl = sstable::SSTable::new(file, self.cache.clone())?;
            new_level.total_size += tbl.size();
            new_level.tables.push(tbl);
            need_update = true;
        }

        for old_tbl in &old_level.tables {
            let id = old_tbl.id();
            if del_ids.contains(&id) {
                del_files.insert(id);
                need_update = true;
            } else {
                new_level.total_size += old_tbl.size();
                new_level.tables.push(old_tbl.clone());
            }
        }
        if !need_update {
            return Ok(());
        }
        new_level
            .tables
            .sort_by(|a, b| a.smallest().cmp(b.smallest()));
        assert_tables_order(&new_level.tables);
        if !cas_resource(&shard.cfs[cf], g, shared, new_scf) {
            error!("there maybe concurrent apply compaction.");
            panic!("failed to update level_handler")
        }
        Ok(())
    }

    fn apply_split_files<'a>(
        &self,
        shard: &'a Shard,
        g: &'a epoch::Guard,
        cs: pb::ChangeSet,
    ) -> Result<()> {
        if shard.get_split_stage() != pb::SplitStage::PreSplitFlushDone {
            error!(
                "wrong split stage for apply split files {:?}",
                shard.get_split_stage()
            );
            return Err(Error::WrongSplitStage);
        }
        let split_files = cs.get_split_files();
        let (old_l0s_shared, old_l0s) = load_resource_with_shared(&shard.l0_tbls, g);
        let mut new_l0s = L0Tables::new(vec![]);
        let fs_opts = dfs::Options::new(shard.id, shard.ver);
        for l0 in split_files.get_l0_creates() {
            let file = self.fs.open(l0.id, fs_opts)?;
            let l0 = sstable::L0Table::new(file, self.cache.clone())?;
            new_l0s.tbls.push(l0);
        }
        for old_l0 in &old_l0s.tbls {
            if split_files.table_deletes.contains(&old_l0.id()) {
                self.fs.remove(old_l0.id(), fs_opts);
            } else {
                new_l0s.tbls.push(old_l0.clone());
            }
        }
        new_l0s
            .tbls
            .sort_by(|a, b| b.commit_ts().cmp(&a.commit_ts()));
        let ok = cas_resource(&shard.l0_tbls, g, old_l0s_shared, new_l0s);
        assert!(ok);
        let mut new_cfs: Vec<ShardCF> = Vec::new();
        for cf in 0..NUM_CFS {
            let max_level = self.opts.cfs[cf].max_levels;
            let shard_cf = ShardCF::new(max_level);
            new_cfs.push(shard_cf);
        }
        for tbl in split_files.get_table_creates() {
            let cf = tbl.cf as usize;
            let scf = &mut new_cfs[cf];
            let level = tbl.level as usize;
            let mut new_handler = &mut scf.levels[level - 1];
            let file = self.fs.open(tbl.id, fs_opts)?;
            new_handler.total_size += file.size();
            let table = sstable::SSTable::new(file, self.cache.clone())?;
            new_handler.tables.push(table);
        }
        new_cfs.reverse();
        for cf in 0..NUM_CFS {
            let mut new_cf = new_cfs.pop().unwrap();
            let max_level = self.opts.cfs[cf].max_levels;
            let (old_shared, old_cf) = load_resource_with_shared(&shard.cfs[cf], g);
            let old_cf = unsafe { old_shared.deref() };
            for level in 1..=max_level {
                let old_handler = &old_cf.levels[level - 1];
                let new_handler = &mut new_cf.levels[level - 1];
                for old_tbl in &old_handler.tables {
                    if split_files.table_deletes.contains(&old_tbl.id()) {
                        self.fs.remove(old_tbl.id(), fs_opts);
                    } else {
                        new_handler.total_size += old_tbl.size();
                        new_handler.tables.push(old_tbl.clone());
                    }
                }
                new_handler
                    .tables
                    .sort_by(|i, j| i.smallest().cmp(j.smallest()));
            }
            cas_resource(&shard.cfs[cf], g, old_shared, new_cf);
        }
        shard.set_split_stage(cs.get_stage());
        Ok(())
    }

    fn pre_load_files(&self, cs: &pb::ChangeSet) -> Result<()> {
        let mut ids = vec![];
        if cs.has_flush() {
            let flush = cs.get_flush();
            if flush.has_l0_create() {
                ids.push(flush.get_l0_create().id);
            }
        }
        if cs.has_compaction() {
            let comp = cs.get_compaction();
            if !is_move_down(comp) {
                for tbl in &comp.table_creates {
                    ids.push(tbl.id);
                }
            }
        }
        if cs.has_split_files() {
            let split_files = cs.get_split_files();
            for l0 in split_files.get_l0_creates() {
                ids.push(l0.id);
            }
            for ln in split_files.get_table_creates() {
                ids.push(ln.id);
            }
        }
        if cs.has_snapshot() {
            let snap = cs.get_snapshot();
            for l0 in snap.get_l0_creates() {
                ids.push(l0.id);
            }
            for ln in snap.get_table_creates() {
                ids.push(ln.id);
            }
        }
        let length = ids.len();
        let (result_tx, result_rx) = crossbeam::channel::bounded(length);
        for id in ids {
            let fs = self.fs.clone();
            let opts = dfs::Options::new(cs.shard_id, cs.shard_ver);
            let tx = result_tx.clone();
            self.fs.get_future_pool().spawn_ok(async move {
                let res = fs.prefetch(id, opts).await;
                tx.send(res).unwrap();
            })
        }
        for _ in 0..length {
            result_rx.recv().unwrap()?;
        }
        Ok(())
    }
}

pub(crate) fn assert_tables_order(tables: &Vec<SSTable>) {
    if tables.len() <= 1 {
        return;
    }
    for i in 0..(tables.len() - 1) {
        let ti = &tables[i];
        let tj = &tables[i + 1];
        if ti.smallest() > ti.biggest()
            || ti.smallest() >= tj.smallest()
            || ti.biggest() >= tj.biggest()
        {
            error!(
                "ti:{}[{:x?},{:x?}], tj:{}[{:x?}, {:x?}]",
                ti.id(),
                ti.smallest(),
                ti.biggest(),
                tj.id(),
                tj.smallest(),
                tj.biggest()
            );
            panic!("the order of tables is invalid")
        }
    }
}