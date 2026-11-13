use serde::{Deserialize, Serialize};
use colossus_types::BlockHeight;
use colossus_ledger::SharedReplicatedLedger;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub method: String,
    pub params: serde_json::Value,
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    pub id: u64,
}

pub struct RpcServer {
    port: u16,
    running: bool,
    chain_height: BlockHeight,
    chain_max_interest_rate_bps: u32,
    active_interest_rate_proposal: Option<serde_json::Value>,
}

impl RpcServer {
    pub fn new(port: u16) -> Self {
        RpcServer {
            port,
            running: false,
            chain_height: BlockHeight(0),
            chain_max_interest_rate_bps: colossus_ledger::DEFAULT_CHAIN_MAX_INTEREST_RATE_BPS,
            active_interest_rate_proposal: None,
        }
    }
    
    pub fn start(&mut self) -> colossus_types::Result<()> {
        self.running = true;
        Ok(())
    }
    
    pub fn stop(&mut self) {
        self.running = false;
    }
    
    pub fn set_height(&mut self, h: BlockHeight) {
        self.chain_height = h;
    }

    pub fn sync_ledger_state(&mut self, ledger: &SharedReplicatedLedger) {
        self.chain_height = BlockHeight(ledger.blocks.len() as u64);
        self.chain_max_interest_rate_bps = ledger.chain_max_interest_rate_bps;
        self.active_interest_rate_proposal = ledger.active_chain_interest_rate_proposal.as_ref().map(|p| {
            serde_json::json!({
                "proposal_id": hex::encode(p.proposal_id.0),
                "proposer": hex::encode(p.proposer.0),
                "new_rate_bps": p.new_rate_bps,
                "created_height": p.created_height.0,
                "approvals": p.approval_count(),
                "votes": p.votes.len(),
            })
        });
    }
    
    pub fn call(&self, req: RpcRequest) -> RpcResponse {
        if !self.running && req.method != "colossus_start" && req.method != "colossus_isRunning" {
            return RpcResponse {
                result: None,
                error: Some("RPC server not running".to_string()),
                id: req.id,
            };
        }
        match req.method.as_str() {
            "colossus_getBlockByHeight" => {
                let height = req.params.get("height").and_then(|v| v.as_u64()).unwrap_or(self.chain_height.0);
                RpcResponse {
                    result: Some(serde_json::json!({"height": height, "transactions": [], "state_root": "0x0"})),
                    error: None,
                    id: req.id,
                }
            },
            "colossus_getBalance" => RpcResponse {
                result: Some(serde_json::json!({"balance": "0", "nonce": 0})),
                error: None,
                id: req.id,
            },
            "colossus_getLedger" => RpcResponse {
                result: Some(serde_json::json!({"ledger": null})),
                error: None,
                id: req.id,
            },
            "colossus_getChainInfo" => RpcResponse {
                result: Some(serde_json::json!({
                    "chain_id": "colossus-1",
                    "height": self.chain_height.0,
                    "consensus": "CustomAttestedBundle",
                    "chain_max_interest_rate_bps": self.chain_max_interest_rate_bps,
                    "active_interest_rate_proposal": self.active_interest_rate_proposal
                })),
                error: None,
                id: req.id,
            },
            "colossus_submitTransaction" => {
                let tx_hash = blake3::hash(&[]);
                RpcResponse {
                    result: Some(serde_json::json!({"tx_hash": hex::encode(tx_hash.as_bytes()), "accepted": true})),
                    error: None,
                    id: req.id,
                }
            },
            "colossus_isRunning" => RpcResponse {
                result: Some(serde_json::json!({"running": self.running})),
                error: None,
                id: req.id,
            },
            "colossus_getPeers" => RpcResponse {
                result: Some(serde_json::json!({"peers": []})),
                error: None,
                id: req.id,
            },
            _ => RpcResponse {
                result: None,
                error: Some(format!("Method '{}' not found", req.method)),
                id: req.id,
            },
        }
    }
}
