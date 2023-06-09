//!
//! A pre-built simple entry to the `rt-evm` development framework.
//!

#![cfg_attr(feature = "benchmark", allow(warnings))]

pub use rt_evm_api as api;
pub use rt_evm_blockmgmt as blockmgmt;
pub use rt_evm_executor as executor;
pub use rt_evm_mempool as mempool;
pub use rt_evm_model as model;
pub use rt_evm_storage as storage;

pub use model::types::{transaction::SignedTransaction as SignedTx, H160 as Address};

use api::{run_jsonrpc_server, DefaultAPIAdapter as API};
use blockmgmt::BlockMgmt;
use executor::RTEvmExecutorAdapter;
use mempool::Mempool;
use model::{
    traits::BlockStorage as _,
    types::{Basic, Block, H160, U256},
};
use once_cell::sync::Lazy;
use ruc::*;
use std::{fs, io::ErrorKind, mem::size_of, path::PathBuf, sync::Arc};
use storage::{MptStore, Storage};

static META_PATH: Lazy<MetaPath> = Lazy::new(|| {
    let mut trie = vsdb::vsdb_get_custom_dir().to_path_buf();
    let mut storage = trie.clone();
    let mut chain_id = trie.clone();

    chain_id.push("EVM_RUNTIME_chain_id.meta");
    trie.push("EVM_RUNTIME_trie.meta");
    storage.push("EVM_RUNTIME_storage.meta");

    MetaPath {
        chain_id,
        trie,
        storage,
    }
});

pub struct EvmRuntime {
    chain_id: u64,

    // create a new instance every time
    mempool: Arc<Mempool>,

    trie: Arc<MptStore>,
    storage: Arc<Storage>,
}

impl EvmRuntime {
    fn new(chain_id: u64, t: MptStore, s: Storage) -> Self {
        #[cfg(not(feature = "benchmark"))]
        const MEM_POOL_CAP: u64 = 20_0000;

        #[cfg(feature = "benchmark")]
        const MEM_POOL_CAP: u64 = 200_0000;

        let trie = Arc::new(t);
        let storage = Arc::new(s);

        Self {
            chain_id,
            mempool: Mempool::new(
                MEM_POOL_CAP,
                600,
                None,
                Arc::clone(&trie),
                Arc::clone(&storage),
                None,
                None,
            ),
            trie,
            storage,
        }
    }

    pub fn create(
        chain_id: u64,
        token_distributions: &[TokenDistributon],
        world_state_cache_size: Option<usize>,
    ) -> Result<Self> {
        let r = Self::new(chain_id, MptStore::new(), Storage::default());

        {
            let mut exector_adapter = RTEvmExecutorAdapter::new(
                &r.trie,
                &r.storage,
                Default::default(),
                world_state_cache_size,
            )
            .c(d!())?;

            token_distributions
                .iter()
                .fold(map! {}, |mut acc, td| {
                    let hdr = acc.entry(td.address).or_insert(*td);
                    if td.amount != hdr.amount {
                        hdr.amount = hdr.amount.saturating_add(td.amount);
                    }
                    acc
                })
                .into_values()
                .for_each(|td| {
                    exector_adapter.apply(td.address, td.basic(), None, vec![], true);
                });

            // Set up the genesis block.
            //
            // NOTE:
            // we should set the timestamp to a static value,
            // do NOT use the local time of the native machine,
            // or various consensus issues will occur.
            r.storage
                .set_block(Block::mock(chain_id, 0, exector_adapter.commit(), 0))
                .c(d!())?;
        }

        // Only need to write once time !
        fs::write(META_PATH.chain_id.as_path(), u64::to_be_bytes(r.chain_id)).c(d!())?;

        // Only need to write once time !
        bcs::to_bytes(&*r.trie)
            .c(d!())
            .and_then(|bytes| fs::write(META_PATH.trie.as_path(), bytes).c(d!()))?;

        // Only need to write once time !
        bcs::to_bytes(&*r.storage)
            .c(d!())
            .and_then(|bytes| fs::write(META_PATH.storage.as_path(), bytes).c(d!()))?;

        Ok(r)
    }

    pub fn restore() -> Result<Option<Self>> {
        let chain_id = fs::read(META_PATH.chain_id.as_path());
        let trie = fs::read(META_PATH.trie.as_path());
        let storage = fs::read(META_PATH.storage.as_path());

        match (chain_id, trie, storage) {
            (Ok(chain_id), Ok(trie), Ok(storage)) => {
                let chain_id = <[u8; size_of::<u64>()]>::try_from(chain_id)
                    .map_err(|_| eg!("invalid length"))
                    .map(u64::from_be_bytes)?;
                let trie = bcs::from_bytes::<MptStore>(&trie).c(d!())?;
                let storage = bcs::from_bytes::<Storage>(&storage).c(d!())?;
                Ok(Some(Self::new(chain_id, trie, storage)))
            }
            (Err(a), Err(b), Err(c)) => match (a.kind(), b.kind(), c.kind()) {
                (ErrorKind::NotFound, ErrorKind::NotFound, ErrorKind::NotFound) => {
                    Ok(None)
                }
                _ => Err(eg!("bad meta data: {}, {}, {}", a, b, c)),
            },
            (a, b, c) => {
                info_omit!(a);
                info_omit!(b);
                info_omit!(c);
                Err(eg!("bad meta data"))
            }
        }
    }

    pub fn restore_or_create(
        chain_id: u64,
        token_distributions: &[TokenDistributon],
        world_state_cache_size: Option<usize>,
    ) -> Result<Self> {
        if let Some(rt) = Self::restore().c(d!())? {
            Ok(rt)
        } else {
            Self::create(chain_id, token_distributions, world_state_cache_size).c(d!())
        }
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn mempool_handler(&self) -> &Mempool {
        &self.mempool
    }

    pub fn trie_handler(&self) -> &MptStore {
        &self.trie
    }

    pub fn storage_handler(&self) -> &Storage {
        &self.storage
    }

    pub fn copy_mempool_handler(&self) -> Arc<Mempool> {
        Arc::clone(&self.mempool)
    }

    pub fn copy_trie_handler(&self) -> Arc<MptStore> {
        Arc::clone(&self.trie)
    }

    pub fn copy_storage_handler(&self) -> Arc<Storage> {
        Arc::clone(&self.storage)
    }

    pub fn generate_blockproducer(
        &self,
        proposer: H160,
        timestamp: u64,
    ) -> Result<BlockMgmt> {
        BlockMgmt::new(
            proposer,
            timestamp,
            self.copy_mempool_handler(),
            self.copy_trie_handler(),
            self.copy_storage_handler(),
        )
        .c(d!())
    }

    // Check transactions received from other nodes?
    pub fn check_signed_tx(&self, tx: &SignedTx) -> Result<()> {
        self.mempool.tx_check_evm(tx, false)
    }

    // Cache transactions received from other nodes?
    pub fn cache_signed_tx(&self, tx: SignedTx) -> Result<()> {
        self.mempool.tx_insert_evm(tx, false)
    }

    pub async fn spawn_jsonrpc_server(
        &self,
        client_version: &str,
        http_listening_address: Option<&str>,
        ws_listening_address: Option<&str>,
    ) -> Result<()> {
        let api = Arc::new(API::new(
            self.copy_mempool_handler(),
            self.copy_trie_handler(),
            self.copy_storage_handler(),
        ));

        let (http_hdr, ws_hdr) = run_jsonrpc_server(
            api,
            client_version,
            http_listening_address,
            ws_listening_address,
        )
        .await
        .c(d!())?;

        if let Some(hdr) = http_hdr {
            tokio::spawn(async { hdr.await });
        }

        if let Some(hdr) = ws_hdr {
            tokio::spawn(async { hdr.await });
        }

        Ok(())
    }
}

struct MetaPath {
    chain_id: PathBuf,
    trie: PathBuf,
    storage: PathBuf,
}

#[derive(Clone, Copy, Debug)]
pub struct TokenDistributon {
    address: H160,
    amount: U256,
}

impl TokenDistributon {
    pub fn new(address: H160, amount: U256) -> Self {
        Self { address, amount }
    }

    fn basic(&self) -> Basic {
        Basic {
            balance: self.amount,
            nonce: Default::default(),
        }
    }
}
