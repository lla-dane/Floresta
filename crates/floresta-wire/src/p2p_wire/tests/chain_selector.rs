#[cfg(test)]
mod tests_utils {
    use std::collections::HashMap;
    use std::fs::File;
    use std::io;
    use std::io::Cursor;
    use std::io::Read;
    use std::mem::ManuallyDrop;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::Instant;

    use async_std::channel::Receiver;
    use async_std::channel::Sender;
    use async_std::future;
    use async_std::sync::RwLock;
    use async_std::task;
    use bitcoin::blockdata::block::Header;
    use bitcoin::consensus::deserialize;
    use bitcoin::consensus::Decodable;
    use bitcoin::hex::FromHex;
    use bitcoin::p2p::utreexo::UtreexoBlock;
    use bitcoin::p2p::ServiceFlags;
    use bitcoin::BlockHash;
    use floresta_chain::pruned_utreexo::BlockchainInterface;
    use floresta_chain::pruned_utreexo::UpdatableChainstate;
    use floresta_chain::AssumeValidArg;
    use floresta_chain::ChainState;
    use floresta_chain::KvChainStore;
    use rustreexo::accumulator::node_hash::NodeHash;
    use serde::Deserialize;
    use serde::Serialize;

    use crate::mempool::Mempool;
    use crate::node::LocalPeerView;
    use crate::node::NodeNotification;
    use crate::node::NodeRequest;
    use crate::node::PeerStatus;
    use crate::node::UtreexoNode;
    use crate::p2p_wire::chain_selector::ChainSelector;
    use crate::p2p_wire::peer::PeerMessages;
    use crate::UtreexoNodeConfig;

    #[derive(Debug, Deserialize, Serialize)]
    struct UtreexoRoots {
        roots: Option<Vec<String>>,
        numleaves: usize,
    }

    #[derive(Deserialize, Debug)]
    struct Block {
        block: String,
    }
    pub fn get_test_headers() -> Vec<Header> {
        let file = include_bytes!(
            "../../../../floresta-chain/src/pruned_utreexo/testdata/signet_headers.zst"
        );
        let uncompressed: Vec<u8> = zstd::decode_all(std::io::Cursor::new(file)).unwrap();
        let mut cursor = Cursor::new(uncompressed);
        let mut headers: Vec<Header> = Vec::new();
        while let Ok(header) = Header::consensus_decode(&mut cursor) {
            headers.push(header);
        }
        headers
    }

    pub fn get_test_blocks() -> io::Result<HashMap<BlockHash, UtreexoBlock>> {
        let dir = "./src/p2p_wire/tests/test_data/blocks.json";
        let mut contents = String::new();
        File::open(dir)?.read_to_string(&mut contents)?;

        let blocks: Vec<Block> = serde_json::from_str(&contents).expect("JSON NOT WELL-FORMATTED");
        let mut u_blocks: HashMap<BlockHash, UtreexoBlock> = HashMap::new();

        for block_str in blocks {
            let block = Vec::from_hex(&block_str.block).unwrap();
            let block: UtreexoBlock = deserialize(&block).unwrap();
            u_blocks.insert(block.block.block_hash(), block);
        }

        Ok(u_blocks)
    }

    pub fn get_test_filters(dir: &str) -> io::Result<HashMap<BlockHash, Vec<u8>>> {
        let mut contents = String::new();
        File::open(dir)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();

        let roots: Vec<UtreexoRoots> = serde_json::from_str(&contents).expect("JSON: BAD");

        let headers = get_test_headers();

        let mut filters = HashMap::new();

        for root in roots.into_iter() {
            let mut buffer = Vec::new();

            // Serealize leaves as u64 into buffer
            buffer.extend_from_slice(&(root.numleaves as u64).to_le_bytes());

            // Process each root-hash into bytes and append
            for root_hash in root.roots.unwrap() {
                let bytes = Vec::from_hex(&root_hash).unwrap();
                buffer.extend_from_slice(&bytes);
            }

            // Insert the serialised Utreexo-Root along with its corresponding BlockHash in the HashMap
            filters.insert(headers[root.numleaves].block_hash(), buffer);
        }

        Ok(filters)
    }
    pub struct TestPeer {
        headers: Vec<Header>,
        blocks: HashMap<BlockHash, UtreexoBlock>,
        filters: HashMap<BlockHash, Vec<u8>>,
        node_tx: Sender<NodeNotification>,
        node_rx: Receiver<NodeRequest>,
        peer_id: u32,
    }

    impl TestPeer {
        pub fn new(
            node_tx: Sender<NodeNotification>,
            headers: Vec<Header>,
            blocks: HashMap<BlockHash, UtreexoBlock>,
            filters: HashMap<BlockHash, Vec<u8>>,
            node_rx: Receiver<NodeRequest>,
            peer_id: u32,
        ) -> Self {
            TestPeer {
                headers,
                blocks,
                filters,
                node_tx,
                node_rx,
                peer_id,
            }
        }

        ///  FOR KEEPING TRACK OF KINDS OF MESSAGE TYPES
        ///
        ///  NODE REQUESTS:
        ///
        /// GetBlock((Vec<BlockHash>, bool))
        /// GetHeaders(Vec<BlockHash>),         *
        /// GetAddresses,
        /// Shutdown,
        /// BroadcastTransaction(Txid),
        /// MempoolTransaction(Txid),
        /// SendAddresses(Vec<AddrV2Message>),
        /// GetUtreexoState((BlockHash, u32)),
        /// GetFilter((BlockHash, u32))

        /// PEER MESSAGES:
        ///
        /// NewBlock(BlockHash),
        /// Block(UtreexoBlock),
        /// Headers(Vec<BlockHeader>),
        /// Addr(Vec<AddrV2Message>),
        /// Ready(Version),
        /// Disconnected(usize),
        /// NotFound(Inventory),
        /// Transaction(Transaction),
        /// UtreexoState(Vec<u8>),
        /// BlockFilter((BlockHash, floresta_compact_filters::BlockFilter))

        /// NODE NOTIFICATIONS:
        ///
        /// FromPeer(u32, PeerMessages)

        pub async fn run(self) {
            loop {
                let req = self.node_rx.recv().await.unwrap();

                match req {
                    NodeRequest::GetHeaders(hashes) => {
                        let pos = hashes.first().unwrap();
                        let pos = self.headers.iter().position(|h| h.block_hash() == *pos);
                        let headers = match pos {
                            None => vec![],
                            Some(pos) => self.headers[(pos + 1)..].to_vec(),
                        };

                        self.node_tx
                            .send(NodeNotification::FromPeer(
                                self.peer_id,
                                PeerMessages::Headers(headers),
                            ))
                            .await
                            .unwrap();
                    }
                    NodeRequest::GetUtreexoState((hash, _)) => {
                        let filters = self.filters.get(&hash).unwrap().clone();
                        self.node_tx
                            .send(NodeNotification::FromPeer(
                                self.peer_id,
                                PeerMessages::UtreexoState(filters),
                            ))
                            .await
                            .unwrap();
                    }
                    NodeRequest::Shutdown => {
                        return;
                    }
                    NodeRequest::GetBlock((hashes, _)) => {
                        for hash in hashes {
                            let block = self.blocks.get(&hash).unwrap().clone();
                            self.node_tx
                                .send(NodeNotification::FromPeer(
                                    self.peer_id,
                                    PeerMessages::Block(block),
                                ))
                                .await
                                .unwrap();
                        }
                    }

                    _ => {}
                }
            }
        }
    }

    fn create_peer(
        headers: Vec<Header>,
        blocks: HashMap<BlockHash, UtreexoBlock>,
        filters: HashMap<BlockHash, Vec<u8>>,
        node_sender: Sender<NodeNotification>,
        sender: Sender<NodeRequest>,
        node_rcv: Receiver<NodeRequest>,
        peer_id: u32,
    ) -> LocalPeerView {
        let peer = TestPeer::new(node_sender, headers, blocks, filters, node_rcv, peer_id);
        task::spawn(peer.run());

        LocalPeerView {
            address: "127.0.0.1".parse().unwrap(),
            services: ServiceFlags::from(1 << 25),
            user_agent: "/utreexo:0.1.0/".to_string(),
            height: 0,
            state: PeerStatus::Ready,
            channel: sender,
            port: 8333,
            feeler: false,
            banscore: 0,
            address_id: 0,
            _last_message: Instant::now(),
        }
    }

    pub async fn setup_test(
        test_name: &str,
        peers: Vec<(
            Vec<Header>,
            HashMap<BlockHash, UtreexoBlock>,
            HashMap<BlockHash, Vec<u8>>,
        )>,
        pow_fraud_proofs: bool,
        network: floresta_chain::Network,
    ) {
        let datadir = format!("./data/{}.node_test", rand::random::<u32>());
        let chainstore = KvChainStore::new(datadir.clone()).unwrap();
        let mempool = Arc::new(RwLock::new(Mempool::new()));
        let chain = ChainState::new(chainstore, network, AssumeValidArg::Disabled);
        let chain = Arc::new(chain);

        let config = UtreexoNodeConfig {
            network: network.into(),
            pow_fraud_proofs,
            compact_filters: false,
            fixed_peer: None,
            max_banscore: 100,
            max_outbound: 8,
            max_inflight: 10,
            datadir: datadir.clone(),
            proxy: None,
            assume_utreexo: None,
            backfill: false,
        };

        let mut node = UtreexoNode::<ChainSelector, Arc<ChainState<KvChainStore>>>::new(
            config,
            chain.clone(),
            mempool,
            None,
        );

        for (i, peer) in peers.into_iter().enumerate() {
            let (sender, receiver) = async_std::channel::bounded(10);
            let peer = create_peer(
                peer.0,
                peer.1,
                peer.2,
                node.node_tx.clone(),
                sender.clone(),
                receiver,
                i as u32,
            );

            let _peer = peer.clone();

            node.peers.insert(i as u32, peer);
            node.peer_ids.push(i as u32);
            match node.peer_by_service.get_mut(&_peer.services) {
                Some(peer_vec) => peer_vec.push(i as u32),
                None => {
                    node.peer_by_service.insert(_peer.services, vec![i as u32]);
                    ()
                }
            }
        }

        let mut node = ManuallyDrop::new(Box::new(node));

        let kill_signal = Arc::new(RwLock::new(false));
        // FIXME: This doesn't look very safe, but we need to coerce a &mut reference of the node
        //        to live for the static lifetime, or it can't be spawn-ed by async-std::task
        let _node: &'static mut UtreexoNode<ChainSelector, Arc<ChainState<KvChainStore>>> =
            unsafe { std::mem::transmute(&mut **node) };

        future::timeout(Duration::from_secs(2), _node.run(kill_signal))
            .await
            .unwrap()
            .unwrap();

        let headers = get_test_headers();

        match test_name {
            "test_chain_selector" => {
                assert_eq!(chain.is_in_idb(), false);
                assert_eq!(chain.get_best_block().unwrap().0, 2015);
                assert_eq!(
                    chain.get_best_block().unwrap().1,
                    headers[2015].block_hash()
                );
            }

            "two_peers_different_tips" => {
                assert_eq!(chain.is_in_idb(), false);
                assert_eq!(chain.get_best_block().unwrap().0, 2014);
                assert_eq!(
                    chain.get_best_block().unwrap().1,
                    headers[2014].block_hash()
                );
            }
            "ten_peers_different_tips" => {
                assert_eq!(chain.is_in_idb(), false);
                assert_eq!(chain.get_best_block().unwrap().0, 2013);
                assert_eq!(
                    chain.get_best_block().unwrap().1,
                    headers[2013].block_hash()
                );
            }
            "two_peers_one_lying" => {
                assert_eq!(chain.is_in_idb(), false);
                assert_eq!(
                    chain.get_root_hashes()[3],
                    NodeHash::from_str(
                        "bfe030a7a994b921fb2329ff085bd0f2351cb5fa251985d6646aaf57954b782b"
                    )
                    .unwrap()
                );
                assert_eq!(chain.get_root_hashes().len(), 6);
                assert_eq!(chain.get_best_block().unwrap().1, headers[119].block_hash());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bitcoin::consensus::deserialize;
    use bitcoin::hex::FromHex;
    use bitcoin::p2p::utreexo::UtreexoBlock;

    use super::tests_utils::get_test_blocks;
    use super::tests_utils::get_test_filters;
    use super::tests_utils::get_test_headers;
    use super::tests_utils::setup_test;

    #[async_std::test]
    async fn accept_one_header() {
        let headers = get_test_headers();

        let block = "00000020f61eee3b63a380a477a063af32b2bbc97c9ff9f01f2c4225e973988108000000f575c83235984e7dc4afc1f30944c170462e84437ab6f2d52e16878a79e4678bd1914d5fae77031eccf4070001010000000001010000000000000000000000000000000000000000000000000000000000000000ffffffff025151feffffff0200f2052a010000001600149243f727dd5343293eb83174324019ec16c2630f0000000000000000776a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf94c4fecc7daa2490047304402205e423a8754336ca99dbe16509b877ef1bf98d008836c725005b3c787c41ebe46022047246e4467ad7cc7f1ad98662afcaf14c115e0095a227c7b05c5182591c23e7e0100012000000000000000000000000000000000000000000000000000000000000000000000000000000000";
        let block = Vec::from_hex(block).unwrap();
        let block: UtreexoBlock = deserialize(&block).unwrap();

        println!("{:?}", block);

        setup_test(
            "test_chain_selector",
            vec![(headers, HashMap::new(), HashMap::new())],
            false,
            floresta_chain::Network::Signet,
        )
        .await;
    }

    #[async_std::test]
    async fn two_peers_different_tips() {
        let mut headers = get_test_headers();

        let mut peers = Vec::new();

        for _ in 0..2 {
            headers.pop();
            peers.push((headers.clone(), HashMap::new(), HashMap::new()))
        }

        setup_test(
            "two_peers_different_tips",
            peers,
            false,
            floresta_chain::Network::Signet,
        )
        .await;
    }

    #[async_std::test]
    async fn ten_peers_different_tips() {
        let mut headers = get_test_headers();

        let mut peers = Vec::new();

        for _ in 0..10 {
            headers.pop();
            headers.pop();

            peers.push((headers.clone(), HashMap::new(), HashMap::new()))
        }

        setup_test(
            "ten_peers_different_tips",
            peers,
            false,
            floresta_chain::Network::Signet,
        )
        .await;
    }

    #[async_std::test]
    async fn two_peers_one_lying() {
        let mut headers = get_test_headers();
        headers.truncate(120);

        let true_filters = get_test_filters("./src/p2p_wire/tests/test_data/roots.json").unwrap();
        let false_filters =
            get_test_filters("./src/p2p_wire/tests/test_data/false_roots.json").unwrap();

        let blocks = get_test_blocks().unwrap();

        let peers = vec![
            (headers.clone(), blocks.clone(), true_filters),
            (headers, blocks, false_filters),
        ];

        setup_test(
            "two_peers_one_lying",
            peers,
            true,
            floresta_chain::Network::Signet,
        )
        .await;
    }

    // two peers in different tips      *

    // 10 peers on different tips       *

    // two-peers pow fraud proofs one lying

    // 10 peers pow fraud proofs, all but one lying (in the chain with less work)

    // two peers, one invalid accumulator

    // 10 peers, only one valid accumulator
}
