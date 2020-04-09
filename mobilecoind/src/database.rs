// Copyright (c) 2018-2020 MobileCoin Inc.

//! The mobilecoind database

use crate::{
    error::Error,
    monitor_store::{MonitorData, MonitorId, MonitorStore},
    subaddress_store::{SubaddressId, SubaddressSPKId, SubaddressStore},
    utxo_store::{UtxoId, UtxoStore},
};

use crate::utxo_store::UnspentTxOut;
use common::{
    logger::{log, Logger},
    HashMap,
};
use lmdb::{Environment, Transaction};
use std::{path::Path, sync::Arc};
use transaction::ring_signature::KeyImage;

// LMDB Constants

const MAX_LMDB_FILE_SIZE: usize = 1_099_511_627_776; // 1 TB

#[derive(Clone)]
pub struct Database {
    // LMDB Environment (database).
    env: Arc<Environment>,

    /// Monitor store.
    monitor_store: MonitorStore,

    /// Subaddress store.
    subaddress_store: SubaddressStore,

    /// Utxo store.
    utxo_store: UtxoStore,

    /// Logger.
    logger: Logger,
}

impl Database {
    pub fn new<P: AsRef<Path>>(path: P, logger: Logger) -> Result<Self, Error> {
        let env = Arc::new(
            Environment::new()
                .set_max_dbs(10)
                .set_map_size(MAX_LMDB_FILE_SIZE)
                .open(path.as_ref())?,
        );

        let monitor_store = MonitorStore::new(env.clone(), logger.clone())?;
        let subaddress_store = SubaddressStore::new(env.clone(), logger.clone())?;
        let utxo_store = UtxoStore::new(env.clone(), logger.clone())?;

        Ok(Self {
            env,
            monitor_store,
            subaddress_store,
            utxo_store,
            logger,
        })
    }

    pub fn add_monitor(&self, data: &MonitorData) -> Result<MonitorId, Error> {
        common::trace_time!(self.logger, "add_monitor");

        let mut db_txn = self.env.begin_rw_txn()?;
        let id = self.monitor_store.add(&mut db_txn, data)?;

        //for index in 0..data.num_subaddresses {
        for index in data.subaddress_indexes() {
            self.subaddress_store
                .insert(&mut db_txn, &id, data, index)?;
        }

        db_txn.commit()?;
        Ok(id)
    }

    pub fn remove_monitor(&self, id: &MonitorId) -> Result<(), Error> {
        common::trace_time!(self.logger, "remove_monitor");

        let mut db_txn = self.env.begin_rw_txn()?;

        let data = self.monitor_store.get_data(&db_txn, &id)?;

        for index in data.subaddress_indexes() {
            self.subaddress_store.delete(&mut db_txn, &data, index)?;
            self.utxo_store.remove_utxos(&mut db_txn, id, index)?;
        }

        self.monitor_store.remove(&mut db_txn, id)?;

        db_txn.commit()?;

        Ok(())
    }

    pub fn get_monitor_data(&self, id: &MonitorId) -> Result<MonitorData, Error> {
        let db_txn = self.env.begin_ro_txn()?;
        self.monitor_store.get_data(&db_txn, id)
    }

    pub fn get_monitor_map(&self) -> Result<HashMap<MonitorId, MonitorData>, Error> {
        let db_txn = self.env.begin_ro_txn()?;
        self.monitor_store.get_map(&db_txn)
    }

    pub fn get_monitor_ids(&self) -> Result<Vec<MonitorId>, Error> {
        let db_txn = self.env.begin_ro_txn()?;
        self.monitor_store.get_ids(&db_txn)
    }

    pub fn get_subaddress_id_by_spk(
        &self,
        subaddress_spk: &SubaddressSPKId,
    ) -> Result<SubaddressId, Error> {
        let db_txn = self.env.begin_ro_txn()?;
        self.subaddress_store
            .get_index_data(&db_txn, subaddress_spk)
    }

    pub fn get_subaddress_id_by_utxo_id(&self, utxo_id: &UtxoId) -> Result<SubaddressId, Error> {
        let db_txn = self.env.begin_ro_txn()?;
        self.utxo_store
            .get_subaddress_id_by_utxo_id(&db_txn, utxo_id)
    }

    pub fn get_utxos_for_subaddress(
        &self,
        monitor_id: &MonitorId,
        index: u64,
    ) -> Result<Vec<UnspentTxOut>, Error> {
        let db_txn = self.env.begin_ro_txn()?;
        self.utxo_store.get_utxos(&db_txn, monitor_id, index)
    }

    pub fn update_attempted_spend(
        &self,
        utxo_ids: &[UtxoId],
        attempted_spend_height: u64,
        attempted_spend_tombstone: u64,
    ) -> Result<(), Error> {
        let mut db_txn = self.env.begin_rw_txn()?;

        self.utxo_store.update_attempted_spend(
            &mut db_txn,
            utxo_ids,
            attempted_spend_height,
            attempted_spend_tombstone,
        )?;

        db_txn.commit()?;

        Ok(())
    }

    /// Feed data processed from a given block into the various stores.
    pub fn block_processed(
        &self,
        monitor_id: &MonitorId,
        block_num: u64,
        discovered_utxos: &[UnspentTxOut],
        spent_key_images: &[KeyImage],
    ) -> Result<(), Error> {
        let mut db_txn = self.env.begin_rw_txn()?;

        // Get monitor data.
        let mut monitor_data = self.monitor_store.get_data(&db_txn, monitor_id)?;

        // If the block being handed to us is not the one we expect, error out.
        if block_num != monitor_data.next_block {
            return Err(Error::InvalidArgument(
                "block_num".to_string(),
                format!(
                    "Expected block {}, got block {}",
                    monitor_data.next_block, block_num
                ),
            ));
        }

        // Store new utxos
        for utxo in discovered_utxos {
            self.utxo_store
                .append_utxo(&mut db_txn, &monitor_id, utxo.subaddress_index, &utxo)?;
        }

        // Remove spent utxos
        let removed_key_images = self.utxo_store.remove_utxos_by_key_images(
            &mut db_txn,
            monitor_id,
            spent_key_images,
        )?;

        // Update monitor data.
        monitor_data.next_block += 1;
        self.monitor_store
            .set_data(&mut db_txn, monitor_id, &monitor_data)?;

        // Commit.
        db_txn.commit()?;

        // Success.
        if discovered_utxos.is_empty() && removed_key_images.is_empty() {
            log::debug!(
                self.logger,
                "Processed {} utxos and {} key images in block {} for monitor id {}",
                discovered_utxos.len(),
                removed_key_images.len(),
                block_num,
                monitor_id
            )
        } else {
            log::info!(
                self.logger,
                "Processed {} utxos and {} key images in block {} for monitor id {}",
                discovered_utxos.len(),
                removed_key_images.len(),
                block_num,
                monitor_id
            )
        };
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{error::Error, test_utils::get_test_databases};
    use common::logger::{test_with_logger, Logger};
    use rand::{rngs::StdRng, SeedableRng};
    use transaction::account_keys::AccountKey;

    // Inserting a monitor that overlaps subaddresses of another monitor should result in an error.
    #[test_with_logger]
    fn test_overlapping_add_monitor_fails(logger: Logger) {
        let mut rng: StdRng = SeedableRng::from_seed([123u8; 32]);

        // Set up a db with 3 random recipients and 10 blocks.
        let (_ledger_db, mobilecoind_db) =
            get_test_databases(3, &vec![], 10, logger.clone(), &mut rng);

        // A test accouunt.
        let account_key = AccountKey::random(&mut rng);

        // Insert the first monitor, with subaddresses 0-9 (inclusive).
        let initial_data = MonitorData::new(
            account_key.clone(),
            0,  // first_subaddress
            10, // num_subaddresses
            0,  // first_block
        )
        .unwrap();

        let monitor_id = mobilecoind_db
            .add_monitor(&initial_data)
            .expect("failed adding monitor");

        // Inserting an identical monitor should fail.
        let data = MonitorData::new(
            account_key.clone(),
            0,  // first_subaddress
            10, // num_subaddresses
            0,  // first_block
        )
        .unwrap();

        match mobilecoind_db.add_monitor(&data) {
            Ok(_) => panic!("unexpected success!"),
            Err(Error::MonitorIdExists) => {}
            Err(err) => panic!("unexpected error {:?}", err),
        };

        // Inserting a monitor with overlapping subaddresses should fail.
        let data = MonitorData::new(
            account_key.clone(),
            5,  // first_subaddress
            10, // num_subaddresses
            0,  // first_block
        )
        .unwrap();

        match mobilecoind_db.add_monitor(&data) {
            Ok(_) => panic!("unexpected success!"),
            Err(Error::SubaddressSPKIdExists) => {}
            Err(err) => panic!("unexpected error {:?}", err),
        };

        // Inserting a monitor with overlapping subaddresses and a different `first_block` should
        // fail.
        let data = MonitorData::new(
            account_key.clone(),
            0,  // first_subaddress
            10, // num_subaddresses
            10, // first_block
        )
        .unwrap();

        match mobilecoind_db.add_monitor(&data) {
            Ok(_) => panic!("unexpected success!"),
            Err(Error::SubaddressSPKIdExists) => {}
            Err(err) => panic!("unexpected error {:?}", err),
        };

        // Inserting a monitor with non overlapping subaddresses should succeed.
        let data = MonitorData::new(
            account_key,
            10, // first_subaddress
            10, // num_subaddresses
            0,  // first_block
        )
        .unwrap();

        let _ = mobilecoind_db
            .add_monitor(&data)
            .expect("failed adding monitor");

        // Removing the first monitor and re-adding it should succeed.
        mobilecoind_db
            .remove_monitor(&monitor_id)
            .expect("failed removing monitor");

        let _ = mobilecoind_db
            .add_monitor(&initial_data)
            .expect("failed adding monitor");
    }
}
