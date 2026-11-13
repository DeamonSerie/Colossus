use serde::{Deserialize, Serialize};
use colossus_types::{Address, Amount, AssetId, BlockHeight, Hash, TxId, LedgerId, BundleId, BountyId, MaintainerRegistry};
use std::collections::{BTreeMap, HashMap};
use colossus_debt_ledger::{DebtLedger, Bounty, BountyAuction};

pub const DEFAULT_CHAIN_MAX_INTEREST_RATE_BPS: u32 = 500;

fn default_treasury_bucket() -> HashMap<AssetId, Amount> {
    HashMap::new()
}

fn default_chain_max_interest_rate_bps() -> u32 {
    DEFAULT_CHAIN_MAX_INTEREST_RATE_BPS
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainInterestRateProposal {
    pub proposal_id: Hash,
    pub proposer: Address,
    pub new_rate_bps: u32,
    pub created_height: BlockHeight,
    pub votes: BTreeMap<[u8; 32], bool>,
}

impl ChainInterestRateProposal {
    pub fn approval_count(&self) -> usize {
        self.votes.values().filter(|&&v| v).count()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MerkleTree {
    pub root: Hash,
    pub leaves: Vec<Hash>,
}

impl MerkleTree {
    pub fn new(leaves: Vec<Hash>) -> Self {
        if leaves.is_empty() {
            let empty_hash = blake3::hash(&[]);
            return MerkleTree { root: Hash(empty_hash.into()), leaves: vec![Hash(empty_hash.into())] };
        }
        let mut current = leaves.clone();
        while current.len() > 1 {
            let mut next = Vec::new();
            for chunk in current.chunks(2) {
                let h1 = chunk[0];
                let h2 = if chunk.len() == 2 { chunk[1] } else { chunk[0] };
                let mut combined = Vec::new();
                combined.extend_from_slice(&h1.0);
                combined.extend_from_slice(&h2.0);
                let hash = blake3::hash(&combined);
                next.push(Hash(hash.into()));
            }
            current = next;
        }
        MerkleTree { root: current[0], leaves }
    }
    
    pub fn verify_proof(&self, leaf: &Hash, proof: &[Hash]) -> bool {
        let mut current = *leaf;
        for p in proof {
            let mut combined = Vec::new();
            combined.extend_from_slice(&current.0);
            combined.extend_from_slice(&p.0);
            current = Hash(blake3::hash(&combined).into());
        }
        current == self.root
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockRelevantState {
    pub height: BlockHeight,
    pub touched_accounts: Vec<Address>,
    pub touched_assets: Vec<AssetId>,
    pub touched_ledgers: Vec<LedgerId>,
    pub state_roots: HashMap<String, Hash>,
    pub merkle_tree: MerkleTree,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Transaction {
    Deposit {
        tx_id: TxId,
        from: Address,
        asset_id: AssetId,
        amount: Amount,
        bridge_proof: Vec<u8>,
        #[serde(default)]
        target_ledger_id: Option<LedgerId>,
    },
    Loan {
        tx_id: TxId,
        borrower: Address,
        ledger_id: LedgerId,
        amount: Amount,
        currency_id: AssetId,
    },
    Repay {
        tx_id: TxId,
        borrower: Address,
        ledger_id: LedgerId,
        amount: Amount,
    },
    Liquidate {
        tx_id: TxId,
        ledger_id: LedgerId,
        liquidator: Address,
    },
    LedgerTransfer {
        tx_id: TxId,
        ledger_id: LedgerId,
        from: Address,
        to: Address,
        pay_debts_in_one_tx: bool,
    },
    ArbBorrow {
        tx_id: TxId,
        arbitrageur: Address,
        debt_ledger_id: LedgerId,
        pool_id: LedgerId,
    },
    DepositorWithdraw {
        tx_id: TxId,
        ledger_id: LedgerId,
        depositor: Address,
    },
    PlaceBountyBid {
        tx_id: TxId,
        bounty_id: BountyId,
        bidder: Address,
        amount: Amount,
    },
    YieldSplitFreeze {
        tx_id: TxId,
        ledger_id: LedgerId,
        depositor: Address,
    },
    SetMinimumYieldSplitters {
        tx_id: TxId,
        ledger_id: LedgerId,
        depositor: Address,
        minimum: u32,
    },
    YieldSplitVote {
        tx_id: TxId,
        ledger_id: LedgerId,
        voter: Address,
        percentage_bps: u32,
    },
    LedgerStarterSale {
        tx_id: TxId,
        ledger_id: LedgerId,
        seller: Address,
        buyer: Address,
        accepted: bool,
    },
    ProposeChainMaxInterestRate {
        tx_id: TxId,
        proposer: Address,
        new_rate_bps: u32,
    },
    VoteChainMaxInterestRate {
        tx_id: TxId,
        voter: Address,
        proposal_id: Hash,
        approve: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub height: BlockHeight,
    pub parent_hash: Hash,
    pub timestamp: u64,
    pub transactions: Vec<Transaction>,
    pub state_root: Hash,
    pub receipts_root: Hash,
    pub bundle_id: BundleId,
    pub bundler: Address,
    pub validator: Address,
    pub committee_attestations: Vec<Vec<u8>>,
    pub base_fee: Amount,
    pub pai_reward: Amount,
    pub gas_used: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountState {
    pub address: Address,
    pub balances: HashMap<AssetId, Amount>,
    pub nonce: u64,
    pub bond: Amount,
    pub bonded_role: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedReplicatedLedger {
    pub blocks: Vec<Block>,
    pub state: HashMap<Address, AccountState>,
    pub assets: HashMap<AssetId, Amount>,
    pub ledgers: HashMap<LedgerId, DebtLedger>,
    pub bounties: HashMap<BountyId, Bounty>,
    pub bounty_auctions: HashMap<BountyId, BountyAuction>,
    pub bonded_set: HashMap<Address, (Amount, String)>,
    pub reputation: HashMap<Address, i64>,
    #[serde(default)]
    pub taker_score: HashMap<Address, i64>,
    #[serde(default)]
    pub taker_scores: HashMap<Address, i64>,
    #[serde(default)]
    pub lender_score: HashMap<Address, i64>,
    #[serde(default)]
    pub lender_scores: HashMap<Address, i64>,
    #[serde(default)]
    pub maintainer_registry: MaintainerRegistry,
    #[serde(default = "default_chain_max_interest_rate_bps")]
    pub chain_max_interest_rate_bps: u32,
    #[serde(default)]
    pub active_chain_interest_rate_proposal: Option<ChainInterestRateProposal>,
    #[serde(default = "default_treasury_bucket")]
    pub treasury_bucket: HashMap<AssetId, Amount>,
}

impl Default for SharedReplicatedLedger {
    fn default() -> Self {
        SharedReplicatedLedger {
            blocks: Vec::new(),
            state: HashMap::new(),
            assets: HashMap::new(),
            ledgers: HashMap::new(),
            bounties: HashMap::new(),
            bounty_auctions: HashMap::new(),
            bonded_set: HashMap::new(),
            reputation: HashMap::new(),
            taker_score: HashMap::new(),
            taker_scores: HashMap::new(),
            lender_score: HashMap::new(),
            lender_scores: HashMap::new(),
            maintainer_registry: MaintainerRegistry::default(),
            chain_max_interest_rate_bps: DEFAULT_CHAIN_MAX_INTEREST_RATE_BPS,
            active_chain_interest_rate_proposal: None,
            treasury_bucket: HashMap::new(),
        }
    }
}

impl SharedReplicatedLedger {
    pub fn is_maintainer(&self, addr: &Address) -> bool {
        self.maintainer_registry.is_maintainer(addr)
    }

    pub fn owner_for_ledger(&self, id: &LedgerId) -> Address {
        self.maintainer_registry.owner_for(id)
    }

    pub fn append_block(&mut self, block: Block) -> colossus_types::Result<()> {
        if let Some(last) = self.blocks.last() {
            if block.height.0 != last.height.0 + 1 {
                return Err(colossus_types::ColossusError::InvalidBlock);
            }
        }
        self.blocks.push(block);
        Ok(())
    }

    /// Protocol-only credit path for the immutable treasury bucket.
    /// Treasury is a permanent sink: no debit/transfer txs/APIs exist.
    pub fn credit_treasury(&mut self, asset: AssetId, amount: Amount) {
        if amount.0 == 0 {
            return;
        }
        // Reflect treasury-held supply in global asset accounting.
        let entry = self.assets.entry(asset).or_insert(Amount(0));
        entry.0 = entry.0.saturating_add(amount.0);
        let t = self.treasury_bucket.entry(asset).or_insert(Amount(0));
        t.0 = t.0.saturating_add(amount.0);
    }
    
    pub fn compute_state_root(&self) -> Hash {
        let mut data = Vec::new();
        data.extend_from_slice(&(self.blocks.len() as u64).to_le_bytes());
        data.extend_from_slice(&(self.ledgers.len() as u64).to_le_bytes());
        data.extend_from_slice(&(self.state.len() as u64).to_le_bytes());
        data.extend_from_slice(&(self.taker_score.len() as u64).to_le_bytes());
        data.extend_from_slice(&(self.taker_scores.len() as u64).to_le_bytes());
        data.extend_from_slice(&(self.lender_score.len() as u64).to_le_bytes());
        data.extend_from_slice(&(self.lender_scores.len() as u64).to_le_bytes());
        data.extend_from_slice(&self.maintainer_registry.fingerprint().0);
        data.extend_from_slice(&self.chain_max_interest_rate_bps.to_le_bytes());
        if let Some(p) = &self.active_chain_interest_rate_proposal {
            data.extend_from_slice(&p.proposal_id.0);
            data.extend_from_slice(&p.proposer.0);
            data.extend_from_slice(&p.new_rate_bps.to_le_bytes());
            data.extend_from_slice(&p.created_height.0.to_le_bytes());
            data.extend_from_slice(&(p.votes.len() as u64).to_le_bytes());
            for (addr, approve) in &p.votes {
                data.extend_from_slice(addr);
                data.push(if *approve { 1 } else { 0 });
            }
        } else {
            data.push(0);
        }
        // Deterministic treasury fingerprint (sorted by asset id).
        data.extend_from_slice(&(self.treasury_bucket.len() as u64).to_le_bytes());
        let mut treasury_items: Vec<_> = self.treasury_bucket.iter().collect();
        treasury_items.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
        for (asset, amt) in treasury_items {
            data.extend_from_slice(&asset.0);
            data.extend_from_slice(&amt.0.to_le_bytes());
        }
        Hash(blake3::hash(&data).into())
    }
    
    pub fn get_block_relevant_state(&self, txs: &[Transaction]) -> BlockRelevantState {
        let mut touched_accounts = Vec::new();
        let mut touched_assets = Vec::new();
        let mut touched_ledgers = Vec::new();
        for tx in txs {
            match tx {
                Transaction::Deposit { from, asset_id, target_ledger_id, .. } => {
                    touched_accounts.push(*from);
                    touched_assets.push(*asset_id);
                    if let Some(ledger_id) = target_ledger_id {
                        touched_ledgers.push(*ledger_id);
                    }
                }
                Transaction::Loan { borrower, ledger_id, currency_id, .. } => {
                    touched_accounts.push(*borrower);
                    touched_ledgers.push(*ledger_id);
                    touched_assets.push(*currency_id);
                }
                Transaction::Repay { borrower, ledger_id, .. } => {
                    touched_accounts.push(*borrower);
                    touched_ledgers.push(*ledger_id);
                    if let Some(n) = self.ledgers.get(ledger_id) {
                        touched_accounts.push(n.owner);
                        touched_accounts.push(n.depositor);
                    }
                }
                Transaction::Liquidate { ledger_id, liquidator, .. } => {
                    touched_accounts.push(*liquidator);
                    touched_ledgers.push(*ledger_id);
                }
                Transaction::LedgerTransfer { from, to, ledger_id, .. } => {
                    touched_accounts.push(*from);
                    touched_accounts.push(*to);
                    touched_ledgers.push(*ledger_id);
                }
                Transaction::ArbBorrow { arbitrageur, debt_ledger_id, pool_id, .. } => {
                    touched_accounts.push(*arbitrageur);
                    touched_ledgers.push(*debt_ledger_id);
                    touched_ledgers.push(*pool_id);
                }
                Transaction::DepositorWithdraw { depositor, ledger_id, .. } => {
                    touched_accounts.push(*depositor);
                    touched_ledgers.push(*ledger_id);
                    if let Some(n) = self.ledgers.get(ledger_id) {
                        touched_accounts.push(n.owner);
                    }
                }
                Transaction::PlaceBountyBid { bidder, .. } => {
                    touched_accounts.push(*bidder);
                }
                Transaction::YieldSplitFreeze { depositor, ledger_id, .. } => {
                    touched_accounts.push(*depositor);
                    touched_ledgers.push(*ledger_id);
                }
                Transaction::SetMinimumYieldSplitters { depositor, ledger_id, .. } => {
                    touched_accounts.push(*depositor);
                    touched_ledgers.push(*ledger_id);
                }
                Transaction::YieldSplitVote { voter, ledger_id, .. } => {
                    touched_accounts.push(*voter);
                    touched_ledgers.push(*ledger_id);
                }
                Transaction::LedgerStarterSale { seller, buyer, ledger_id, .. } => {
                    touched_accounts.push(*seller);
                    touched_accounts.push(*buyer);
                    touched_ledgers.push(*ledger_id);
                    if let Some(n) = self.ledgers.get(ledger_id) {
                        touched_accounts.push(n.depositor);
                    }
                }
                Transaction::ProposeChainMaxInterestRate { proposer, .. } => {
                    touched_accounts.push(*proposer);
                }
                Transaction::VoteChainMaxInterestRate { voter, .. } => {
                    touched_accounts.push(*voter);
                }
            }
        }
        touched_accounts.sort_by(|a,b| a.0.iter().cmp(b.0.iter()));
        touched_accounts.dedup_by(|a,b| a.0 == b.0);
        touched_ledgers.sort_by(|a,b| a.0.iter().cmp(b.0.iter()));
        touched_ledgers.dedup_by(|a,b| a.0 == b.0);
        touched_assets.sort_by(|a,b| a.0.iter().cmp(b.0.iter()));
        touched_assets.dedup_by(|a,b| a.0 == b.0);
        
        let mut leaves = Vec::new();
        for acc in &touched_accounts {
            leaves.push(Hash(blake3::hash(&acc.0).into()));
        }
        for nft in &touched_ledgers {
            leaves.push(Hash(blake3::hash(&nft.0).into()));
        }
        if leaves.is_empty() {
            leaves.push(Hash(blake3::hash(&[]).into()));
        }
        
        BlockRelevantState {
            height: BlockHeight(self.blocks.len() as u64),
            touched_accounts,
            touched_assets,
            touched_ledgers,
            state_roots: HashMap::new(),
            merkle_tree: MerkleTree::new(leaves),
        }
    }
}

impl SharedReplicatedLedger {
    pub fn process_bounty_expiry<H: Into<BlockHeight>>(&mut self, height: H) {
        let height = height.into();
        let ids: Vec<BountyId> = self.bounties.iter().filter(|(_, b)| b.end_height == height).map(|(id, _)| *id).collect();
        for id in ids {
            let highest = self.bounty_auctions.get(&id).and_then(|a| a.highest.clone());
            if let Some(h) = highest {
                if let Some(bounty) = self.bounties.get(&id) {
                    let currency_id = bounty.currency_id;
                    let current_amount = bounty.current_amount;
                    let avg_interest = bounty.avg_interest;
                    let reward_wallet = bounty.reward_wallet;
                    let winner = h.bidder;
                    {
                        let entry = self.assets.entry(currency_id).or_insert(Amount(0));
                        entry.0 = entry.0.saturating_add(current_amount.0);
                    }
                    {
                        let acc = self.state.entry(winner).or_insert_with(|| AccountState {
                            address: winner, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                        });
                        acc.balances.entry(currency_id).and_modify(|e| e.0 += current_amount.0).or_insert(current_amount);
                    }
                    let pai_id = AssetId([1u8; 32]);
                    let pay_amount = avg_interest;
                    {
                        if let Some(acc) = self.state.get_mut(&winner) {
                            if let Some(bal) = acc.balances.get_mut(&pai_id) {
                                bal.0 = bal.0.saturating_sub(pay_amount.0);
                            }
                        }
                    }
                    {
                        let acc = self.state.entry(reward_wallet).or_insert_with(|| AccountState {
                            address: reward_wallet, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                        });
                        acc.balances.entry(pai_id).and_modify(|e| e.0 += pay_amount.0).or_insert(pay_amount);
                    }
                    if let Some(b) = self.bounties.get_mut(&id) {
                        b.winner = Some(winner);
                        b.winning_bid = Some(h.amount);
                        b.status = colossus_debt_ledger::BountyStatus::Settled;
                    }
                }
            } else {
                let (original_amount, _currency_id, reward_wallet) = {
                    if let Some(b) = self.bounties.get(&id) {
                        (b.original_amount, b.currency_id, b.reward_wallet)
                    } else { (Amount(0), AssetId([0u8;32]), Address::zero()) }
                };
                let dist_total = Amount(original_amount.0 * 20 / 100);
                let wallets: Vec<Address> = self.state.keys().cloned().collect();
                let n = wallets.len().max(1) as u128;
                let per = Amount(dist_total.0 / n);
                let pai_id = AssetId([1u8; 32]);
                for w in wallets {
                    let acc = self.state.entry(w).or_insert_with(|| AccountState {
                        address: w, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                    });
                    acc.balances.entry(pai_id).and_modify(|e| e.0 += per.0).or_insert(per);
                }
                {
                    let entry = self.assets.entry(pai_id).or_insert(Amount(0));
                    entry.0 = entry.0.saturating_add(dist_total.0);
                }
                let burn_remaining = Amount(original_amount.0.saturating_sub(dist_total.0));
                if let Some(b) = self.bounties.get_mut(&id) {
                    b.burn_remaining = burn_remaining;
                    b.status = colossus_debt_ledger::BountyStatus::ExpiredNoBids;
                }
                self.adjust_lender_score(reward_wallet, -10);
            }
        }
    }

    pub fn process_timed_burn<H: Into<BlockHeight>>(&mut self, _height: H) {
        let ids: Vec<BountyId> = self.bounties.iter().filter(|(_, b)| b.status == colossus_debt_ledger::BountyStatus::ExpiredNoBids && b.burn_remaining.0 > 0).map(|(id, _)| *id).collect();
        for id in ids {
            let currency_id = { let b = self.bounties.get(&id).unwrap(); b.currency_id };
            let burn_per_block = { let b = self.bounties.get(&id).unwrap(); b.burn_per_block };
            let burn_amt = { let b = self.bounties.get(&id).unwrap(); std::cmp::min(burn_per_block.0, b.burn_remaining.0) };
            if burn_amt > 0 {
                {
                    let entry = self.assets.entry(currency_id).or_insert(Amount(0));
                    entry.0 = entry.0.saturating_sub(burn_amt);
                }
                if let Some(b) = self.bounties.get_mut(&id) {
                    b.burn_remaining.0 = b.burn_remaining.0.saturating_sub(burn_amt);
                    if b.burn_remaining.0 == 0 {
                        b.status = colossus_debt_ledger::BountyStatus::Burned;
                    }
                }
            }
        }
    }
}

impl SharedReplicatedLedger {
    pub fn taker_score(&self, addr: Address) -> i64 {
        self.taker_score.get(&addr).copied().unwrap_or(0)
    }
    pub fn lender_score(&self, addr: Address) -> i64 {
        self.lender_score.get(&addr).copied().unwrap_or(0)
    }
    pub fn adjust_taker_score(&mut self, addr: Address, delta: i64) {
        let cur = self.taker_score.get(&addr).copied().unwrap_or(0);
        let mut v = cur + delta;
        if v < 0 { v = 0; }
        if v > 100 { v = 100; }
        self.taker_score.insert(addr, v);
        self.taker_scores.insert(addr, v);
    }
    pub fn adjust_lender_score(&mut self, addr: Address, delta: i64) {
        let cur = self.lender_score.get(&addr).copied().unwrap_or(0);
        let mut v = cur + delta;
        if v < 0 { v = 0; }
        if v > 100 { v = 100; }
        self.lender_score.insert(addr, v);
        self.lender_scores.insert(addr, v);
    }
}

pub fn process_bounty_expiry<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) {
    let height = height.into();
    ledger.process_bounty_expiry(height);
}

pub fn process_timed_burn<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) {
    let height = height.into();
    ledger.process_timed_burn(height);
}

pub fn process_bounty_burn<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) {
    let height = height.into();
    ledger.process_timed_burn(height);
}

pub fn process_burn<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) {
    let height = height.into();
    ledger.process_timed_burn(height);
}

pub fn process_bounties<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) {
    let height = height.into();
    ledger.process_bounty_expiry(height);
    ledger.process_timed_burn(height);
}
