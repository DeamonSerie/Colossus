use colossus_types::{Result, ColossusError};
use colossus_ledger::{SharedReplicatedLedger, Transaction};
use colossus_debt_ledger::{DebtLedger, DebtEvent, LedgerStatus, WithdrawalRecord, WithdrawalStatus, LoanRef, TrackedFund, TrackedFundStatus, ObligationStatus, LossEvent, RepaymentRecord};
use colossus_types::{LedgerId, Amount, BlockHeight, Address, AssetId, TxId, BountyId};

pub struct Executor;

pub const LOANABILITY_THRESHOLD: i128 = 30_000_000_000_000_000;
pub const MATURITY_BLOCKS: u64 = 100;
pub const TAKER_PARTIAL_DELTA: i64 = 2;
pub const TAKER_FULL_BONUS: i64 = 8;
pub const LENDER_MATURE_LOST_LOANABILITY_DELTA: i64 = 10;
pub const LENDER_EARLY_WITHDRAWAL_DELTA: i64 = -5;
pub const LENDER_MATURE_WITH_LOANABILITY_DELTA: i64 = 3;
pub const LENDER_FAILED_BOUNTY_DELTA: i64 = -10;
pub const YIELD_SPLIT_DEFAULT_PERCENTAGE_BPS: u32 = 1000;
pub const STARTER_SALE_PAYMENT_BPS: u32 = 4000;
pub const YIELD_SPLIT_RECURRING_PERCENT: u64 = 10;
pub const YIELD_SPLIT_RECURRING_INTERVAL: u32 = 6;
pub const YIELD_SPLIT_TAKER_DELTA: i64 = 8;
pub const YIELD_SPLITTER_MIN_TAKER_SCORE: i64 = 1;

impl Executor {
    fn effective_rate(nft: &DebtLedger, chain_max_bps: u32) -> u32 {
        nft.interest_rate_bps.min(chain_max_bps)
    }

    fn interest_capped(nft: &DebtLedger, principal: Amount, chain_max_bps: u32) -> Amount {
        Amount((principal.0 * (Self::effective_rate(nft, chain_max_bps) as u128)) / 10000)
    }

    fn try_apply_chain_max_rate_proposal(ledger: &mut SharedReplicatedLedger) {
        let apply = if let Some(p) = &ledger.active_chain_interest_rate_proposal {
            let approvals = p.votes.values().filter(|&&v| v).count();
            approvals * 2 > ledger.maintainer_registry.members.len()
        } else {
            false
        };
        if apply {
            if let Some(p) = ledger.active_chain_interest_rate_proposal.take() {
                ledger.chain_max_interest_rate_bps = p.new_rate_bps;
            }
        }
    }

    pub fn execute_deposit(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::Deposit { tx_id, from, asset_id, amount, target_ledger_id, .. } = tx {
            if let Some(target) = target_ledger_id {
                let height = BlockHeight(ledger.blocks.len() as u64);
                let nft = ledger.ledgers.get_mut(target).ok_or(ColossusError::LedgerNotFound)?;
                if nft.frozen {
                    return Err(ColossusError::LedgerFrozen);
                }
                if nft.status == LedgerStatus::Burned {
                    return Err(ColossusError::CannotTransferBurnedLedger);
                }
                if nft.depositor != *from {
                    return Err(ColossusError::InvalidTransaction);
                }
                if nft.collateral_asset != *asset_id {
                    return Err(ColossusError::InvalidTransaction);
                }
                let loan_count = nft.loan_count;
                let loanable_pct: u128 = (70i32 - (loan_count as i32) * 5).clamp(20, 70) as u128;
                let new_loanable = Amount(amount.0 * loanable_pct / 100);
                let interest_add = Amount(amount.0 * 10 / 100);
                nft.deposit_amount = Amount(nft.deposit_amount.0.saturating_add(amount.0));
                nft.total_minted_from_ledger = Amount(nft.total_minted_from_ledger.0.saturating_add(new_loanable.0));
                nft.deposit_loanability = Amount(nft.deposit_loanability.0.saturating_add(new_loanable.0));
                nft.interest_pool = Amount(nft.interest_pool.0.saturating_add(interest_add.0));
                nft.available_collateral_backing = Amount(nft.available_collateral_backing.0.saturating_add(amount.0.saturating_sub(new_loanable.0)));
                let consistency_score = if !nft.affected_loans.is_empty() {
                    let total = nft.affected_loans.len() as f64;
                    let repaid = nft.affected_loans.iter().filter(|al| al.paid_at_start.0 > 0).count() as f64;
                    if total > 0.0 { repaid / total } else { 0.0 }
                } else {
                    let total = nft.cumulative_repaid.0.saturating_add(nft.debt_balance.0);
                    if total > 0 { nft.cumulative_repaid.0 as f64 / total as f64 } else { 0.0 }
                };
                let consistency_bps = (consistency_score * 10000.0).round() as u128;
                let interest_bonus = Amount(interest_add.0 * consistency_bps / 10000);
                nft.deposit_interest_accrued = Amount(nft.deposit_interest_accrued.0.saturating_add(interest_bonus.0));
                nft.add_debt_event(DebtEvent::DepositConsistencyInterest {
                    tx_id: *tx_id,
                    amount: interest_bonus,
                    consistency_score,
                    height,
                });
                nft.add_debt_event(DebtEvent::Deposit { tx_id: *tx_id, amount: *amount, height });
                nft.update_value_factor();
            } else {
                let ledger_id = LedgerId(blake3::hash(&[from.0.as_slice(), tx_id.0.as_slice()].concat()).into());
                let owner = ledger.maintainer_registry.owner_for(&ledger_id);
                let rate_bps = ledger.chain_max_interest_rate_bps;
                let mut nft = DebtLedger::new(
                    ledger_id,
                    owner,
                    *from,
                    *asset_id,
                    *amount,
                    *asset_id,
                    rate_bps,
                    BlockHeight(ledger.blocks.len() as u64),
                );
                nft.add_debt_event(DebtEvent::Deposit { tx_id: *tx_id, amount: *amount, height: BlockHeight(ledger.blocks.len() as u64) });
                let loan_count: u32 = 0;
                let loanable_pct: u128 = (70i32 - (loan_count as i32) * 5).clamp(20, 70) as u128;
                let loanable = Amount(amount.0 * loanable_pct / 100);
                let interest_pool = Amount(amount.0 * 10 / 100);
                nft.total_minted_from_ledger = loanable;
                nft.deposit_loanability = loanable;
                nft.interest_pool = interest_pool;
                nft.available_collateral_backing = Amount(amount.0.saturating_sub(loanable.0));
                nft.update_value_factor();
                ledger.ledgers.insert(ledger_id, nft);
            }
        }
        Ok(())
    }
    
    pub fn execute_ledger_transfer(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::LedgerTransfer { tx_id, ledger_id, from, to, .. } = tx {
            if ledger.ledgers.get(ledger_id).map(|n| n.frozen).unwrap_or(false) {
                return Err(ColossusError::LedgerFrozen);
            }
            if !ledger.maintainer_registry.is_maintainer(to) {
                return Err(ColossusError::UnauthorizedMaintainerOwner);
            }
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                if nft.status == LedgerStatus::Burned {
                    return Err(ColossusError::CannotTransferBurnedLedger);
                }
                if nft.owner != *from {
                    return Err(ColossusError::UnauthorizedMaintainerOwner);
                }
                let chain_max = ledger.chain_max_interest_rate_bps;
                let total_debt = nft.total_debt_with_interest_capped(chain_max);
                if total_debt.0 > 0 {
                    nft.burn("Failed to pay debts in one transaction".to_string(), BlockHeight(ledger.blocks.len() as u64));
                    return Err(ColossusError::MustPayDebtInFull);
                }
                if *from != *to {
                    let previous = nft.deposit_loanability;
                    let new_loanability = Amount(nft.deposit_amount.0 * 70 / 100);
                    nft.add_debt_event(DebtEvent::LoanabilityReset {
                        tx_id: *tx_id,
                        from: *from,
                        to: *to,
                        previous_loanability: previous,
                        new_loanability,
                        height: BlockHeight(ledger.blocks.len() as u64),
                    });
                    nft.deposit_loanability = new_loanability;
                    nft.total_minted_from_ledger = new_loanability;
                    nft.loan_count = 0;
                    nft.loans.clear();
                    nft.matured = false;
                    nft.repayment_records.clear();
                    nft.update_value_factor();
                }
                nft.owner = *to;
                nft.add_debt_event(DebtEvent::Transferred { from: *from, to: *to, height: BlockHeight(ledger.blocks.len() as u64) });
            } else {
                return Err(ColossusError::LedgerNotFound);
            }
        }
        Ok(())
    }
    
    pub fn execute_repay(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::Repay { tx_id, borrower, ledger_id, amount } = tx {
            if ledger.ledgers.get(ledger_id).map(|n| n.frozen).unwrap_or(false) {
                return Err(ColossusError::LedgerFrozen);
            }
            let mut taker_delta: i64 = 0;
            let mut do_taker_score = false;
            let mut pai_yield_credits: Vec<(Address, u128)> = Vec::new();
            let mut starter_interest_credits: Vec<(Address, u128, AssetId)> = Vec::new();
            let mut recurring_transfers: Vec<(Address, Address, u128)> = Vec::new();
            let mut recurring_taker_scores: Vec<Address> = Vec::new();
            let mut depositor_routed: (Address, AssetId, u128, u128) = (Address::zero(), AssetId([0u8;32]), 0, 0);
            let mut treasury_routed: (AssetId, u128) = (AssetId([0u8;32]), 0);
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                if nft.status == LedgerStatus::Burned {
                    return Err(ColossusError::CannotTransferBurnedLedger);
                }
                if nft.status == LedgerStatus::PaidOff {
                    return Ok(());
                }
                let debt_before = nft.debt_balance.0;
                if debt_before == 0 {
                    nft.status = LedgerStatus::PaidOff;
                    nft.update_value_factor();
                    return Ok(());
                }
                let loaned_amount = Amount(debt_before + nft.cumulative_repaid.0);
                let is_full = debt_before <= amount.0;
                let applied = Amount(amount.0.min(debt_before));
                let incremental = nft.repayment_count > 0;
                let was_full_no_prior = nft.repayment_count == 0 && debt_before > 0 && is_full;
                let height = BlockHeight(ledger.blocks.len() as u64);
                let was_consistent_full = !was_full_no_prior
                    && is_full
                    && debt_before > 0
                    && nft.consistent_repayment_history(borrower, *amount, height);
                if is_full {
                    let boost = if incremental { loaned_amount.0 / 10 } else { loaned_amount.0 };
                    nft.available_collateral_backing.0 += boost;
                    nft.add_debt_event(DebtEvent::FullRepaymentBoost { tx_id: *tx_id, loaned_amount, boost_amount: Amount(boost), incremental, height });
                } else {
                    nft.available_collateral_backing.0 += applied.0 / 2;
                }
                if was_full_no_prior || was_consistent_full {
                    if !nft.yield_splitters.contains(borrower) {
                        nft.yield_splitters.push(*borrower);
                        nft.add_debt_event(DebtEvent::YieldSplitterAdded { tx_id: *tx_id, splitter: *borrower, height });
                    }
                    let interest = Self::interest_capped(nft, loaned_amount, ledger.chain_max_interest_rate_bps);
                    let split_amount = interest.0 * (nft.yield_split_percentage_bps as u128) / 10000;
                    pai_yield_credits.push((*borrower, split_amount));
                    let entry = nft.yield_splitter_pending.entry(*borrower).or_insert(Amount(0));
                    entry.0 += split_amount;
                    nft.deposit_interest_accrued.0 = nft.deposit_interest_accrued.0.saturating_sub(split_amount);
                    nft.add_debt_event(DebtEvent::YieldSplitPaid { tx_id: *tx_id, splitter: *borrower, amount: Amount(split_amount), height });
                    let withdrawn = nft.withdrawal_executed;
                    let starter = nft.owner;
                    let currency = nft.loaned_currency_id;
                    let starter_share = interest.0.saturating_sub(split_amount);
                    // Starter boost only when not withdrawn; post-withdraw depositor-entitled remnants handled by routing below.
                    if !withdrawn && starter_share > 0 {
                        nft.add_debt_event(DebtEvent::StarterInterestPaid { tx_id: *tx_id, starter, amount: Amount(starter_share), currency_id: currency, height });
                        starter_interest_credits.push((starter, starter_share, currency));
                    }
                }

                // Route repayment installment (principal + depositor-entitled interest) per withdrawal state.
                let currency = nft.loaned_currency_id;
                let depositor = nft.depositor;
                let mut remaining = applied.0;
                let mut paid_principal: u128 = 0;
                let mut paid_interest: u128 = 0;

                // Ensure affected_loans mirror exists for loans without explicit entries (migration-safe).
                if nft.affected_loans.is_empty() && !nft.loans.is_empty() {
                    for loan in &nft.loans {
                        let loaner = loan.loaner.unwrap_or(depositor);
                        nft.affected_loans.push(AffectedLoan {
                            loan_ref: LoanRef { loan_id: loan.loan_id, loaner },
                            loaner,
                            original_obligation: loan.original_obligation,
                            paid_at_start: Amount(loan.principal_paid_to_depositor.0.min(loan.original_obligation.0)),
                            adjusted_obligation: Amount(loan.original_obligation.0.saturating_sub(loan.principal_paid_to_depositor.0.min(loan.original_obligation.0))),
                            obligation_status: ObligationStatus::Active,
                            released: false,
                        });
                    }
                }

                let withdrawn = nft.withdrawal_executed;
                for loan in nft.loans.iter_mut() {
                    if remaining == 0 { break; }
                    let principal_outstanding = loan.amount.0.saturating_sub(loan.principal_paid_to_depositor.0.min(loan.amount.0));
                    if principal_outstanding > 0 {
                        let ppay = remaining.min(principal_outstanding);
                        loan.principal_paid_to_depositor.0 = loan.principal_paid_to_depositor.0.saturating_add(ppay);
                        paid_principal = paid_principal.saturating_add(ppay);
                        remaining = remaining.saturating_sub(ppay);
                        // update affected loan mirror
                        for al in nft.affected_loans.iter_mut() {
                            if al.loan_ref.loan_id == loan.loan_id {
                                al.paid_at_start.0 = al.paid_at_start.0.saturating_add(ppay).min(al.original_obligation.0);
                                al.adjusted_obligation.0 = al.adjusted_obligation.0.saturating_sub(ppay);
                                if al.adjusted_obligation.0 == 0 && al.obligation_status != ObligationStatus::ReleasedNoPay {
                                    al.obligation_status = ObligationStatus::PartiallyPaid;
                                }
                                break;
                            }
                        }
                    }
                    if remaining == 0 { break; }
                    // Interest attributable to depositor and not yet paid/forfeited.
                    let entitled = loan.interest_entitled.0;
                    let interest_paid = loan.interest_paid_to_depositor.0.min(entitled);
                    let interest_forgone = loan.forfeited_interest_to_treasury.0;
                    let interest_outstanding = entitled.saturating_sub(interest_paid).saturating_sub(interest_forgone);
                    if interest_outstanding > 0 {
                        let ipay = remaining.min(interest_outstanding);
                        remaining = remaining.saturating_sub(ipay);
                        if !withdrawn {
                            loan.interest_paid_to_depositor.0 = loan.interest_paid_to_depositor.0.saturating_add(ipay);
                            paid_interest = paid_interest.saturating_add(ipay);
                        } else {
                            loan.routed_to_treasury_post_withdraw.0 = loan.routed_to_treasury_post_withdraw.0.saturating_add(ipay);
                        }
                    }
                }
                // Any residual (e.g. ledgers without loan breakdown) treated as principal-equivalent routing.
                if remaining > 0 {
                    paid_principal = paid_principal.saturating_add(remaining);
                    remaining = 0;
                }

                if !withdrawn {
                    depositor_routed = (depositor, currency, paid_principal, paid_interest);
                    if paid_principal > 0 || paid_interest > 0 {
                        nft.add_debt_event(DebtEvent::RepaymentRoutedToDepositor {
                            tx_id: *tx_id, depositor, asset: currency, principal: Amount(paid_principal), interest: Amount(paid_interest), height,
                        });
                    }
                } else {
                    let to_treasury = paid_principal.saturating_add(paid_interest);
                    treasury_routed = (currency, to_treasury);
                    if to_treasury > 0 {
                        nft.add_debt_event(DebtEvent::RepaymentRoutedToTreasury {
                            tx_id: *tx_id, asset: currency, amount: Amount(to_treasury), height,
                        });
                    }
                }

                nft.debt_balance.0 = nft.debt_balance.0.saturating_sub(applied.0);
                nft.cumulative_repaid.0 += applied.0;
                nft.repayment_records.push(RepaymentRecord {
                    borrower: *borrower,
                    amount: applied,
                    height,
                });
                nft.repayment_count += 1;
                nft.add_debt_event(DebtEvent::Repayment { tx_id: *tx_id, amount: applied, interest_paid: Amount(paid_interest), height });
                if nft.repayment_count % YIELD_SPLIT_RECURRING_INTERVAL == 0 && nft.yield_split_established {
                    let splitters = nft.yield_splitters.clone();
                    let depositor = nft.depositor;
                    for splitter in splitters {
                        let pending = nft.yield_splitter_pending.get(&splitter).copied().unwrap_or(Amount(0));
                        let share = Amount(pending.0 * (YIELD_SPLIT_RECURRING_PERCENT as u128) / 100);
                        if share.0 > 0 {
                            recurring_transfers.push((splitter, depositor, share.0));
                            nft.add_debt_event(DebtEvent::YieldSplitDistribution { tx_id: *tx_id, depositor, amount: share, height });
                            recurring_taker_scores.push(splitter);
                        }
                    }
                    for splitter in nft.yield_splitters.clone() {
                        nft.yield_splitter_pending.insert(splitter, Amount(0));
                    }
                }
                if nft.debt_balance.0 == 0 {
                    nft.status = LedgerStatus::PaidOff;
                    nft.add_debt_event(DebtEvent::DebtPaidInFull { height });
                }
                if applied.0 > 0 {
                    if debt_before > applied.0 {
                        taker_delta = TAKER_PARTIAL_DELTA;
                    } else {
                        taker_delta = TAKER_FULL_BONUS;
                    }
                    do_taker_score = true;
                }
                nft.update_value_factor();
            } else {
                return Err(ColossusError::LedgerNotFound);
            }
            // apply PAI yield credits
            let pai_id = AssetId([1u8; 32]);
            for (addr, amt) in pai_yield_credits {
                if amt > 0 {
                    {
                        let entry = ledger.assets.entry(pai_id).or_insert(Amount(0));
                        entry.0 = entry.0.saturating_add(amt);
                    }
                    {
                        let acc = ledger.state.entry(addr).or_insert_with(|| colossus_ledger::AccountState {
                            address: addr, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                        });
                        acc.balances.entry(pai_id).and_modify(|e| e.0 += amt).or_insert(Amount(amt));
                    }
                }
            }
            // apply starter interest credits
            for (starter, amt, currency) in starter_interest_credits {
                if amt > 0 {
                    {
                        let entry = ledger.assets.entry(currency).or_insert(Amount(0));
                        entry.0 = entry.0.saturating_add(amt);
                    }
                    {
                        let acc = ledger.state.entry(starter).or_insert_with(|| colossus_ledger::AccountState {
                            address: starter, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                        });
                        acc.balances.entry(currency).and_modify(|e| e.0 += amt).or_insert(Amount(amt));
                    }
                }
            }
            // apply depositor installment routing (pre-withdraw)
            {
                let (depositor, currency, p, i) = depositor_routed;
                let total = p.saturating_add(i);
                if total > 0 {
                    let entry = ledger.assets.entry(currency).or_insert(Amount(0));
                    entry.0 = entry.0.saturating_add(total);
                    let acc = ledger.state.entry(depositor).or_insert_with(|| colossus_ledger::AccountState {
                        address: depositor, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                    });
                    acc.balances.entry(currency).and_modify(|e| e.0 += total).or_insert(Amount(total));
                }
            }
            // apply post-withdraw treasury routing
            {
                let (currency, amt) = treasury_routed;
                if amt > 0 {
                    ledger.credit_treasury(currency, Amount(amt));
                }
            }
            // apply recurring transfers
            for (splitter, depositor, share) in recurring_transfers {
                if let Some(acc) = ledger.state.get_mut(&splitter) {
                    if let Some(bal) = acc.balances.get_mut(&pai_id) {
                        bal.0 = bal.0.saturating_sub(share);
                    }
                }
                {
                    let acc = ledger.state.entry(depositor).or_insert_with(|| colossus_ledger::AccountState {
                        address: depositor, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                    });
                    acc.balances.entry(pai_id).and_modify(|e| e.0 += share).or_insert(Amount(share));
                }
            }
            if do_taker_score {
                ledger.adjust_taker_score(*borrower, taker_delta);
            }
            for splitter in recurring_taker_scores {
                ledger.adjust_taker_score(splitter, YIELD_SPLIT_TAKER_DELTA);
            }
        }
        Ok(())
    }

    pub fn execute_liquidate(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::Liquidate { tx_id: _, ledger_id, liquidator } = tx {
            if ledger.ledgers.get(ledger_id).map(|n| n.frozen).unwrap_or(false) {
                return Err(ColossusError::LedgerFrozen);
            }
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                if nft.status == LedgerStatus::Burned {
                    return Err(ColossusError::CannotTransferBurnedLedger);
                }
                nft.status = LedgerStatus::Liquidating;
                nft.add_debt_event(DebtEvent::LiquidationStarted { tx_id: colossus_types::TxId([0u8; 32]), height: BlockHeight(ledger.blocks.len() as u64) });
                let _ = liquidator;
            } else {
                return Err(ColossusError::LedgerNotFound);
            }
        }
        Ok(())
    }
    
    pub fn execute_depositor_withdraw(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::DepositorWithdraw { tx_id, ledger_id, depositor } = tx {
            if ledger.ledgers.get(ledger_id).map(|n| n.frozen).unwrap_or(false) {
                return Err(ColossusError::LedgerFrozen);
            }
            let height = BlockHeight(ledger.blocks.len() as u64);
            {
                let nft = match ledger.ledgers.get(ledger_id) {
                    Some(n) => n,
                    None => return Err(ColossusError::LedgerNotFound),
                };
                if nft.status == LedgerStatus::Burned { return Err(ColossusError::CannotTransferBurnedLedger); }
                if nft.depositor != *depositor { return Err(ColossusError::InvalidBlock); }
                if nft.withdrawal_executed { return Err(ColossusError::InvalidBlock); }
            }
            let (principal, accrued_interest, minted_currency) = {
                let nft = ledger.ledgers.get(ledger_id).unwrap();
                (nft.deposit_amount, nft.deposit_interest_accrued, nft.loaned_currency_id)
            };
            // All uncollected depositor-entitled loan-generated interest is forfeited to treasury.
            let forfeited_interest = {
                let nft = ledger.ledgers.get(ledger_id).unwrap();
                let mut total = accrued_interest.0;
                for loan in &nft.loans {
                    if loan.depositor_withdrawal_forfeited {
                        continue;
                    }
                    let paid = loan.interest_paid_to_depositor.0.min(loan.interest_entitled.0);
                    let unpaid = loan.interest_entitled.0.saturating_sub(paid);
                    total = total.saturating_add(unpaid);
                }
                Amount(total)
            };

            // Depositor receives principal only; forfeited interest goes to immutable treasury bucket.
            {
                let entry = ledger.assets.entry(minted_currency).or_insert(Amount(0));
                entry.0 += principal.0;
            }
            {
                let depositor_account = ledger.state.entry(*depositor).or_insert_with(|| colossus_ledger::AccountState {
                    address: *depositor, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                });
                depositor_account.balances.entry(minted_currency).and_modify(|e| e.0 += principal.0).or_insert(principal);
            }
            if forfeited_interest.0 > 0 {
                ledger.credit_treasury(minted_currency, forfeited_interest);
            }

            let withdrawal_id = { let nft = ledger.ledgers.get(ledger_id).unwrap(); nft.withdrawals.len() as u64 + 1 };
            let affected_refs: Vec<LoanRef> = {
                let nft = ledger.ledgers.get(ledger_id).unwrap();
                nft.affected_loans.iter().map(|al| al.loan_ref.clone()).collect()
            };
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                nft.add_debt_event(DebtEvent::DepositorWithdraw {
                    tx_id: *tx_id, depositor: *depositor, principal_returned: principal, interest_returned: Amount(0),
                    minted_amount: principal, minted_currency, height,
                });
                if forfeited_interest.0 > 0 {
                    nft.add_debt_event(DebtEvent::DepositInterestForfeitedToTreasury {
                        tx_id: *tx_id,
                        withdrawal_id,
                        asset: minted_currency,
                        amount: forfeited_interest,
                        height,
                    });
                }
                nft.withdrawals.push(WithdrawalRecord {
                    id: withdrawal_id, tx_id: *tx_id, depositor: *depositor, principal_returned: principal, interest_returned: Amount(0),
                    minted_amount: principal, minted_currency, height, affected_loan_refs: affected_refs, status: WithdrawalStatus::Completed,
                });
                nft.withdrawal_executed = true;
                nft.total_withdrawn_minted = Amount(nft.total_withdrawn_minted.0 + principal.0);
                nft.withdrawal_forfeited_interest_total = Amount(nft.withdrawal_forfeited_interest_total.0.saturating_add(forfeited_interest.0));
                nft.deposit_interest_accrued = Amount(0);
                for loan in nft.loans.iter_mut() {
                    if loan.depositor_withdrawal_forfeited {
                        continue;
                    }
                    let paid = loan.interest_paid_to_depositor.0.min(loan.interest_entitled.0);
                    let unpaid = loan.interest_entitled.0.saturating_sub(paid);
                    if unpaid > 0 {
                        loan.forfeited_interest_to_treasury = Amount(loan.forfeited_interest_to_treasury.0.saturating_add(unpaid));
                    }
                    loan.depositor_withdrawal_forfeited = true;
                }
                nft.withdrawn_funds_tracked.push(TrackedFund {
                    id: 1, withdrawal_id, currency_id: minted_currency, amount: principal, holder: Some(*depositor), status: TrackedFundStatus::Held, loss_events: Vec::new(),
                });
                nft.add_debt_event(DebtEvent::WithdrawalFundTracked { tx_id: *tx_id, withdrawal_id, tracked_fund_id: 1, status: TrackedFundStatus::Held, height });
            }
            // Adjust loans separately
            let loan_list: Vec<(LedgerId, Address, Amount)> = {
                let nft = ledger.ledgers.get(ledger_id).unwrap();
                nft.affected_loans.iter().map(|al| (al.loan_ref.loan_id, al.loaner, al.original_obligation)).collect()
            };
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                let mut i = 0;
                while i < nft.affected_loans.len() && i < loan_list.len() {
                    let orig = loan_list[i].2;
                    let adj40 = Amount(orig.0 * 40 / 100);
                    nft.affected_loans[i].adjusted_obligation = adj40;
                    nft.affected_loans[i].obligation_status = ObligationStatus::DiscountedToFortyPercent;
                    nft.affected_loans[i].released = false;
                    let lid = loan_list[i].0;
                    let laddr = loan_list[i].1;
                    nft.add_debt_event(DebtEvent::LoanObligationAdjusted {
                        tx_id: *tx_id, loan_id: lid, loaner: laddr, original_obligation: orig, adjusted_obligation: adj40, adjustment_rate_bps: 4000, height,
                    });
                    i += 1;
                }
            }

            // Lender scoring
            let total_loaned: u128 = ledger.ledgers.get(ledger_id).map(|n| n.affected_loans.iter().map(|al| al.original_obligation.0).sum()).unwrap_or(0);
            let created_at = ledger.ledgers.get(ledger_id).map(|n| n.created_at_height).unwrap_or(BlockHeight(0));
            if total_loaned as i128 <= LOANABILITY_THRESHOLD {
                ledger.adjust_lender_score(*depositor, LENDER_MATURE_LOST_LOANABILITY_DELTA);
            } else if height.0 < created_at.0 + MATURITY_BLOCKS {
                ledger.adjust_lender_score(*depositor, LENDER_EARLY_WITHDRAWAL_DELTA);
            } else {
                ledger.adjust_lender_score(*depositor, LENDER_MATURE_WITH_LOANABILITY_DELTA);
            }

            // Bounty creation if unpaid affected loans exist
            let unpaid_exists = {
                if let Some(nft) = ledger.ledgers.get(ledger_id) {
                    !nft.affected_loans.is_empty() && nft.affected_loans.iter().any(|al| !al.released)
                } else { false }
            };
            if unpaid_exists {
                let (source_ledger_id, reward_wallet, currency_id, original_amount, loan_count, consistency_score, avg_interest) = {
                    let nft = ledger.ledgers.get(ledger_id).unwrap();
                    let chain_max = ledger.chain_max_interest_rate_bps;
                    let loan_count = nft.affected_loans.len() as u32;
                    let repaid = nft.affected_loans.iter().filter(|al| al.paid_at_start.0 > 0).count() as f64;
                    let loan_total = nft.affected_loans.len() as f64;
                    let consistency_score = if loan_total > 0.0 { repaid / loan_total } else { 1.0 };
                    let avg_interest = if nft.affected_loans.is_empty() {
                        Amount(0)
                    } else {
                        let rate = Self::effective_rate(nft, chain_max) as u128;
                        let sum: u128 = nft.affected_loans.iter().map(|al| {
                            (al.original_obligation.0 * rate) / 10000
                        }).sum();
                        Amount(sum / (nft.affected_loans.len() as u128))
                    };
                    // Bounty tracks withdrawn principal amount (forfeited interest is in treasury, not recoverable via depositor funds).
                    (nft.id, nft.depositor, nft.loaned_currency_id, principal, loan_count, consistency_score, avg_interest)
                };
                let base_blocks: u64 = 100;
                let discount: u64 = (consistency_score * 50.0) as u64;
                let time_limit = base_blocks.saturating_sub(discount).max(5);
                let bounty_id = colossus_types::BountyId(blake3::hash(&[tx_id.0.as_slice(), ledger_id.0.as_slice()].concat()).into());
                let bounty = colossus_debt_ledger::Bounty {
                    id: bounty_id,
                    source_ledger_id,
                    reward_wallet,
                    currency_id,
                    original_amount,
                    current_amount: original_amount,
                    created_at_height: height,
                    time_limit_blocks: time_limit,
                    end_height: BlockHeight(height.0 + time_limit),
                    loan_count,
                    consistency_score,
                    avg_interest,
                    burn_remaining: Amount(0),
                    burn_per_block: avg_interest,
                    winner: None,
                    winning_bid: None,
                    status: colossus_debt_ledger::BountyStatus::Open,
                };
                let auction = colossus_debt_ledger::BountyAuction {
                    bounty_id,
                    bids: Vec::new(),
                    highest: None,
                    end_height: BlockHeight(height.0 + time_limit),
                };
                ledger.bounties.insert(bounty_id, bounty);
                ledger.bounty_auctions.insert(bounty_id, auction);
            }
        }
        Ok(())
    }
    
    pub fn execute_yield_split_freeze(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::YieldSplitFreeze { tx_id, ledger_id, depositor } = tx {
            let nft = ledger.ledgers.get(ledger_id).ok_or(ColossusError::LedgerNotFound)?;
            if nft.status == LedgerStatus::Burned {
                return Err(ColossusError::CannotTransferBurnedLedger);
            }
            if nft.depositor != *depositor {
                return Err(ColossusError::InvalidBlock);
            }
            if nft.frozen {
                return Err(ColossusError::InvalidBlock);
            }
            let height = BlockHeight(ledger.blocks.len() as u64);
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                nft.frozen = true;
                nft.add_debt_event(DebtEvent::YieldSplitFrozen { tx_id: *tx_id, depositor: *depositor, height });
            }
        }
        Ok(())
    }

    pub fn execute_set_minimum_yield_splitters(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::SetMinimumYieldSplitters { tx_id, ledger_id, depositor, minimum } = tx {
            let nft = ledger.ledgers.get(ledger_id).ok_or(ColossusError::LedgerNotFound)?;
            if nft.depositor != *depositor {
                return Err(ColossusError::InvalidBlock);
            }
            if !nft.frozen {
                return Err(ColossusError::InvalidBlock);
            }
            let height = BlockHeight(ledger.blocks.len() as u64);
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                nft.minimum_yield_splitter_users = *minimum;
                nft.add_debt_event(DebtEvent::MinimumYieldSplittersSet { tx_id: *tx_id, depositor: *depositor, minimum: *minimum, height });
            }
        }
        Ok(())
    }

    pub fn execute_yield_split_vote(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::YieldSplitVote { tx_id, ledger_id, voter, percentage_bps } = tx {
            let nft = ledger.ledgers.get(ledger_id).ok_or(ColossusError::LedgerNotFound)?;
            if !nft.frozen {
                return Err(ColossusError::InvalidBlock);
            }
            if !nft.yield_splitters.contains(voter) {
                return Err(ColossusError::InvalidBlock);
            }
            if ledger.taker_score(*voter) < YIELD_SPLITTER_MIN_TAKER_SCORE {
                return Err(ColossusError::InvalidBlock);
            }
            let height = BlockHeight(ledger.blocks.len() as u64);
            // record vote
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                nft.yield_votes.insert(*voter, *percentage_bps);
                nft.add_debt_event(DebtEvent::YieldSplitPercentageVoted { tx_id: *tx_id, voter: *voter, percentage_bps: *percentage_bps, height });
            }
            // check majority
            let (should_finalize, winning_pct) = {
                let nft = ledger.ledgers.get(ledger_id).unwrap();
                if nft.yield_votes.len() * 2 > nft.yield_splitters.len() && nft.yield_splitters.len() >= nft.minimum_yield_splitter_users as usize {
                    // tally
                    let mut counts: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
                    for pct in nft.yield_votes.values() {
                        *counts.entry(*pct).or_insert(0) += 1;
                    }
                    let mut best_pct = nft.yield_split_percentage_bps;
                    let mut best_count = 0usize;
                    for (pct, count) in counts.iter() {
                        if *count > best_count || (*count == best_count && *pct > best_pct) {
                            best_pct = *pct;
                            best_count = *count;
                        }
                    }
                    (true, best_pct)
                } else {
                    (false, 0u32)
                }
            };
            if should_finalize {
                if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                    nft.yield_split_percentage_bps = winning_pct;
                    nft.yield_split_established = true;
                    nft.frozen = false;
                    nft.add_debt_event(DebtEvent::YieldSplitEstablished { tx_id: *tx_id, percentage_bps: winning_pct, height });
                }
            }
        }
        Ok(())
    }

    pub fn execute_place_bounty_bid(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::PlaceBountyBid { tx_id: _, bounty_id, bidder, amount } = tx {
            let height = BlockHeight(ledger.blocks.len() as u64);
            let bounty = ledger.bounties.get(bounty_id).ok_or(ColossusError::BountyNotFound)?;
            if bounty.status != colossus_debt_ledger::BountyStatus::Open && bounty.status != colossus_debt_ledger::BountyStatus::Auctioning {
                return Err(ColossusError::BountyAlreadySettled);
            }
            if height.0 >= bounty.end_height.0 {
                return Err(ColossusError::BountyExpired);
            }
            let current_highest = ledger.bounty_auctions.get(bounty_id).and_then(|a| a.highest.clone());
            if let Some(h) = &current_highest {
                if amount.0 <= h.amount.0 {
                    return Err(ColossusError::InvalidBountyBid);
                }
            }
            let currency_id = bounty.currency_id;
            let pai_id = AssetId([1u8; 32]);
            let sufficient = {
                match ledger.state.get(bidder) {
                    Some(acc) => {
                        let pai_bal = acc.balances.get(&pai_id).map(|x| x.0).unwrap_or(0);
                        let cur_bal = acc.balances.get(&currency_id).map(|x| x.0).unwrap_or(0);
                        pai_bal >= amount.0 || cur_bal >= amount.0
                    }
                    None => false,
                }
            };
            if !sufficient {
                return Err(ColossusError::InsufficientBalance);
            }
            let bid = colossus_debt_ledger::BountyBid { bidder: *bidder, amount: *amount, height };
            if let Some(auction) = ledger.bounty_auctions.get_mut(bounty_id) {
                auction.bids.push(bid.clone());
                auction.highest = Some(bid.clone());
            }
            if let Some(b) = ledger.bounties.get_mut(bounty_id) {
                b.winning_bid = Some(*amount);
                b.winner = Some(*bidder);
                b.status = colossus_debt_ledger::BountyStatus::Auctioning;
            }
        }
        Ok(())
    }

    pub fn apply_withdrawn_funds_loss(ledger: &mut SharedReplicatedLedger, ledger_id: &LedgerId, withdrawal_id: u64, total_lost: Amount, tx_id: TxId, reason: String, height: BlockHeight) -> Result<()> {
        // Get currency
        let currency_id = {
            match ledger.ledgers.get(ledger_id) {
                Some(n) => n.loaned_currency_id,
                None => return Err(ColossusError::LedgerNotFound),
            }
        };
        // Mark funds lost
        if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
            for fund in nft.withdrawn_funds_tracked.iter_mut() {
                if fund.withdrawal_id == withdrawal_id {
                    fund.status = TrackedFundStatus::Lost;
                    fund.loss_events.push(LossEvent { height, reason: reason.clone(), amount_lost: total_lost });
                }
            }
            nft.add_debt_event(DebtEvent::WithdrawnFundsLost {
                tx_id,
                withdrawal_id,
                total_lost,
                currency_id,
                height,
            });
        }
        // Check if all lost for this withdrawal
        let mut all_lost_flag = true;
        if let Some(nft) = ledger.ledgers.get(ledger_id) {
            for fund in &nft.withdrawn_funds_tracked {
                if fund.withdrawal_id == withdrawal_id {
                    if fund.status != TrackedFundStatus::Lost {
                        all_lost_flag = false;
                        break;
                    }
                }
            }
        }
        if all_lost_flag {
            // Collect refunds
            let mut refunds = Vec::new();
            if let Some(nft) = ledger.ledgers.get(ledger_id) {
                for affected in &nft.affected_loans {
                    if !affected.released && affected.paid_at_start.0 > 0 {
                        refunds.push((affected.loaner, affected.paid_at_start));
                    }
                }
            }
            for (loaner, refund_amount) in refunds {
                let entry = ledger.assets.entry(currency_id).or_insert(Amount(0));
                entry.0 += refund_amount.0;
                let loaner_account = ledger.state.entry(loaner).or_insert_with(|| colossus_ledger::AccountState {
                    address: loaner,
                    balances: std::collections::HashMap::new(),
                    nonce: 0,
                    bond: Amount(0),
                    bonded_role: None,
                });
                loaner_account.balances.entry(currency_id).and_modify(|e| e.0 += refund_amount.0).or_insert(refund_amount);
                if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                    nft.add_debt_event(DebtEvent::LoanerRefunded {
                        tx_id,
                        loaner,
                        amount_refunded: refund_amount,
                        currency_id,
                        height,
                    });
                }
            }
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                for affected in nft.affected_loans.iter_mut() {
                    if !affected.released {
                        affected.obligation_status = if affected.paid_at_start.0 > 0 { ObligationStatus::Refunded } else { ObligationStatus::ReleasedNoPay };
                        affected.released = true;
                        affected.adjusted_obligation = Amount(0);
                    }
                }
            }
        }
        Ok(())
    }

    
    pub fn process_bounty_expiry<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) {
        let height = height.into();
        process_bounty_expiry(ledger, height);
    }

    pub fn process_timed_burn<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) {
        let height = height.into();
        process_timed_burn(ledger, height);
    }

    pub fn process_bounty_burn<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) {
        let height = height.into();
        process_timed_burn(ledger, height);
    }

    pub fn process_burn<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) {
        let height = height.into();
        process_timed_burn(ledger, height);
    }

    pub fn finalize_block<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) {
        let height = height.into();
        process_bounty_expiry(ledger, height);
        process_timed_burn(ledger, height);
    }

    pub fn execute_ledger_starter_sale(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::LedgerStarterSale { tx_id, ledger_id, seller, buyer, accepted } = tx {
            // frozen check first
            if ledger.ledgers.get(ledger_id).map(|n| n.frozen).unwrap_or(false) {
                return Err(ColossusError::LedgerFrozen);
            }
            // lookup
            {
                let nft = ledger.ledgers.get(ledger_id).ok_or(ColossusError::LedgerNotFound)?;
                if nft.status == LedgerStatus::Burned {
                    return Err(ColossusError::CannotTransferBurnedLedger);
                }
                if nft.owner != *seller {
                    return Err(ColossusError::InvalidTransaction);
                }
                if !nft.most_loans_unpaid() {
                    return Err(ColossusError::StarterSaleNotAllowed);
                }
            }
            let height = BlockHeight(ledger.blocks.len() as u64);

            if !*accepted {
                if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                    nft.add_debt_event(DebtEvent::StarterSaleDenied {
                        tx_id: *tx_id,
                        seller: *seller,
                        buyer: *buyer,
                        height,
                    });
                }
                return Ok(());
            }

            if !ledger.maintainer_registry.is_maintainer(buyer) {
                return Err(ColossusError::UnauthorizedMaintainerOwner);
            }
            // compute amounts
            let unpaid_debt = ledger.ledgers.get(ledger_id).unwrap().debt_balance;
            let payment_total = Amount(unpaid_debt.0 * STARTER_SALE_PAYMENT_BPS as u128 / 10000);
            let share = payment_total.0 / 3;
            let mut depositor_share = payment_total.0 - 2 * share;
            let buyer_share = share;
            let mut loaner_share = share;

            let loaned_currency_id = ledger.ledgers.get(ledger_id).unwrap().loaned_currency_id;
            let depositor = ledger.ledgers.get(ledger_id).unwrap().depositor;

            // Buyer credit
            let buyer_ledgers: Vec<LedgerId> = ledger.ledgers.iter()
                .filter(|(id, n)| **id != *ledger_id && n.owner == *buyer && n.debt_balance.0 > 0)
                .map(|(id, _)| *id)
                .collect();
            if !buyer_ledgers.is_empty() {
                let total_buyer_debt: u128 = buyer_ledgers.iter()
                    .map(|id| ledger.ledgers.get(id).unwrap().debt_balance.0)
                    .sum();
                if total_buyer_debt > 0 {
                    for id in buyer_ledgers.iter() {
                        let debt = ledger.ledgers.get(id).unwrap().debt_balance.0;
                        let portion = buyer_share * debt / total_buyer_debt;
                        if let Some(n) = ledger.ledgers.get_mut(id) {
                            n.debt_balance.0 = n.debt_balance.0.saturating_sub(portion);
                        }
                    }
                }
            } else if buyer_share > 0 {
                {
                    let entry = ledger.assets.entry(loaned_currency_id).or_insert(Amount(0));
                    entry.0 = entry.0.saturating_add(buyer_share);
                }
                {
                    let acc = ledger.state.entry(*buyer).or_insert_with(|| colossus_ledger::AccountState {
                        address: *buyer, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                    });
                    acc.balances.entry(loaned_currency_id).and_modify(|e| e.0 += buyer_share).or_insert(Amount(buyer_share));
                }
            }

            // Determine unreleased affected loans & denom
            let (unreleased_indices, denom) = {
                let nft = ledger.ledgers.get(ledger_id).unwrap();
                let mut idxs = Vec::new();
                for (i, al) in nft.affected_loans.iter().enumerate() {
                    if !al.released {
                        idxs.push(i);
                    }
                }
                let d: u128 = idxs.iter().map(|&i| nft.affected_loans[i].original_obligation.0).sum();
                (idxs, d)
            };

            let redirect_loaner = unreleased_indices.is_empty() || denom == 0;
            if redirect_loaner {
                depositor_share += loaner_share;
                loaner_share = 0;
            }

            // Depositor credit
            if depositor_share > 0 {
                {
                    let entry = ledger.assets.entry(loaned_currency_id).or_insert(Amount(0));
                    entry.0 = entry.0.saturating_add(depositor_share);
                }
                {
                    let acc = ledger.state.entry(depositor).or_insert_with(|| colossus_ledger::AccountState {
                        address: depositor, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                    });
                    acc.balances.entry(loaned_currency_id).and_modify(|e| e.0 += depositor_share).or_insert(Amount(depositor_share));
                }
                if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                    nft.deposit_interest_accrued.0 = nft.deposit_interest_accrued.0.saturating_sub(depositor_share);
                }
            }

            // Loaner credit
            if loaner_share > 0 && !redirect_loaner {
                // need clone loaner info first
                let loaner_info: Vec<(usize, Address, u128)> = {
                    let nft = ledger.ledgers.get(ledger_id).unwrap();
                    unreleased_indices.iter().map(|&i| (i, nft.affected_loans[i].loaner, nft.affected_loans[i].original_obligation.0)).collect()
                };
                for (i, loaner, orig) in loaner_info {
                    let portion = if denom > 0 { loaner_share * orig / denom } else { 0 };
                    if portion > 0 {
                        {
                            let entry = ledger.assets.entry(loaned_currency_id).or_insert(Amount(0));
                            entry.0 = entry.0.saturating_add(portion);
                        }
                        {
                            let acc = ledger.state.entry(loaner).or_insert_with(|| colossus_ledger::AccountState {
                                address: loaner, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                            });
                            acc.balances.entry(loaned_currency_id).and_modify(|e| e.0 += portion).or_insert(Amount(portion));
                        }
                    }
                    if portion > 0 {
                        if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                            let al = &mut nft.affected_loans[i];
                            al.adjusted_obligation.0 = al.adjusted_obligation.0.saturating_sub(portion);
                            if al.adjusted_obligation.0 == 0 {
                                al.released = true;
                                al.obligation_status = ObligationStatus::ReleasedNoPay;
                            } else {
                                al.obligation_status = ObligationStatus::PartiallyPaid;
                            }
                        }
                    }
                }
            }

            // Reduce sold NFT debt
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                nft.debt_balance.0 = nft.debt_balance.0.saturating_sub(payment_total.0);
                nft.cumulative_repaid.0 = nft.cumulative_repaid.0.saturating_add(payment_total.0);
            }

            // Loanability reset if seller != buyer
            if *seller != *buyer {
                let previous = ledger.ledgers.get(ledger_id).unwrap().deposit_loanability;
                let new_loanability = Amount(ledger.ledgers.get(ledger_id).unwrap().deposit_amount.0 * 70 / 100);
                if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                    nft.add_debt_event(DebtEvent::LoanabilityReset {
                        tx_id: *tx_id,
                        from: *seller,
                        to: *buyer,
                        previous_loanability: previous,
                        new_loanability,
                        height,
                    });
                    nft.deposit_loanability = new_loanability;
                    nft.total_minted_from_ledger = new_loanability;
                    nft.loan_count = 0;
                    nft.loans.clear();
                    nft.matured = false;
                    nft.repayment_records.clear();
                    nft.update_value_factor();
                }
            }

            // Set owner
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                nft.owner = *buyer;
            }

            // Bounty creation
            let has_unreleased = ledger.ledgers.get(ledger_id).map(|n| n.affected_loans.iter().any(|al| !al.released)).unwrap_or(false);
            let remaining_debt_positive = ledger.ledgers.get(ledger_id).map(|n| n.debt_balance.0 > 0).unwrap_or(false);
            if has_unreleased && remaining_debt_positive {
                let (source_ledger_id, reward_wallet, currency_id, remaining_debt, loan_count, consistency_score, avg_interest) = {
                    let nft = ledger.ledgers.get(ledger_id).unwrap();
                    let chain_max = ledger.chain_max_interest_rate_bps;
                    let loan_count = nft.affected_loans.iter().filter(|al| !al.released).count() as u32;
                    let repaid = nft.affected_loans.iter().filter(|al| al.paid_at_start.0 > 0).count() as f64;
                    let loan_total = nft.affected_loans.len() as f64;
                    let consistency_score = if loan_total > 0.0 { repaid / loan_total } else { 1.0 };
                    let avg_interest = if nft.affected_loans.is_empty() {
                        Amount(0)
                    } else {
                        let rate = Self::effective_rate(nft, chain_max) as u128;
                        let sum: u128 = nft.affected_loans.iter().map(|al| {
                            (al.original_obligation.0 * rate) / 10000
                        }).sum();
                        Amount(sum / (nft.affected_loans.len() as u128))
                    };
                    (nft.id, nft.depositor, nft.loaned_currency_id, nft.debt_balance, loan_count, consistency_score, avg_interest)
                };
                let base_blocks: u64 = 100;
                let discount: u64 = (consistency_score * 50.0) as u64;
                let time_limit = base_blocks.saturating_sub(discount).max(5);
                let bounty_id = colossus_types::BountyId(blake3::hash(&[tx_id.0.as_slice(), ledger_id.0.as_slice()].concat()).into());
                let bounty = colossus_debt_ledger::Bounty {
                    id: bounty_id,
                    source_ledger_id,
                    reward_wallet,
                    currency_id,
                    original_amount: remaining_debt,
                    current_amount: remaining_debt,
                    created_at_height: height,
                    time_limit_blocks: time_limit,
                    end_height: BlockHeight(height.0 + time_limit),
                    loan_count,
                    consistency_score,
                    avg_interest,
                    burn_remaining: Amount(0),
                    burn_per_block: avg_interest,
                    winner: None,
                    winning_bid: None,
                    status: colossus_debt_ledger::BountyStatus::Open,
                };
                let auction = colossus_debt_ledger::BountyAuction {
                    bounty_id,
                    bids: Vec::new(),
                    highest: None,
                    end_height: BlockHeight(height.0 + time_limit),
                };
                ledger.bounties.insert(bounty_id, bounty);
                ledger.bounty_auctions.insert(bounty_id, auction);
            }

            // Record events
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                nft.add_debt_event(DebtEvent::Transferred { from: *seller, to: *buyer, height });
                nft.add_debt_event(DebtEvent::StarterSaleCompleted {
                    tx_id: *tx_id,
                    seller: *seller,
                    buyer: *buyer,
                    unpaid_debt,
                    payment_amount: payment_total,
                    height,
                });
            }
        }
        Ok(())
    }

    pub fn execute_arb_borrow(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::ArbBorrow { tx_id: _, arbitrageur, debt_ledger_id, pool_id } = tx {
            if !ledger.ledgers.contains_key(debt_ledger_id) {
                return Err(ColossusError::LedgerNotFound);
            }
            let _ = (arbitrageur, pool_id);
        }
        Ok(())
    }

    pub fn execute_loan(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::Loan { tx_id, borrower, ledger_id, amount, currency_id } = tx {
            let height = BlockHeight(ledger.blocks.len() as u64);
            let chain_max = ledger.chain_max_interest_rate_bps;
            let nft = ledger.ledgers.get(ledger_id).ok_or(ColossusError::LedgerNotFound)?;
            if nft.frozen {
                return Err(ColossusError::LedgerFrozen);
            }
            if nft.status == LedgerStatus::Burned {
                return Err(ColossusError::CannotTransferBurnedLedger);
            }
            if nft.matured {
                return Err(ColossusError::InvalidTransaction);
            }
            let secondary = nft.loan_count > 0 || nft.debt_balance.0 > 0;
            let interest = if secondary && !nft.matured {
                Self::interest_capped(nft, *amount, chain_max)
            } else {
                Amount(0)
            };
            let disbursed = Amount(amount.0.saturating_sub(interest.0));
            if amount.0 == 0 || disbursed.0 == 0 {
                return Err(ColossusError::InvalidTransaction);
            }
            if disbursed.0 > nft.deposit_loanability.0 {
                return Err(ColossusError::InvalidTransaction);
            }
            let mint_currency = if secondary { nft.loaned_currency_id } else { *currency_id };
            let interest_rate_bps = Self::effective_rate(nft, chain_max);
            let loan_id = LedgerId(blake3::hash(&[borrower.0.as_slice(), tx_id.0.as_slice(), ledger_id.0.as_slice()].concat()).into());
            if let Some(nft) = ledger.ledgers.get_mut(ledger_id) {
                nft.debt_balance.0 = nft.debt_balance.0.saturating_add(disbursed.0);
                nft.loan_count = nft.loan_count.saturating_add(1);
                nft.deposit_loanability = Amount(nft.deposit_loanability.0.saturating_sub(disbursed.0));
                nft.total_minted_from_ledger = Amount(nft.total_minted_from_ledger.0.saturating_add(disbursed.0));

                let mut loan = colossus_lending::Loan::new(loan_id, *ledger_id, *borrower, disbursed, interest_rate_bps, height);
                loan.loan_count = nft.loan_count;
                // Attribute depositor-entitled interest for this loan (deterministic, audited).
                let entitled = Amount((disbursed.0 * (interest_rate_bps as u128)) / 10000);
                loan.interest_entitled = entitled;
                loan.accrued_interest = entitled;
                loan.loaner = Some(nft.depositor);
                loan.original_obligation = disbursed;
                nft.loans.push(loan);

                // Production affected_loans population for deterministic withdraw/repay routing.
                nft.affected_loans.push(colossus_debt_ledger::AffectedLoan {
                    loan_ref: colossus_debt_ledger::LoanRef { loan_id, loaner: nft.depositor },
                    loaner: nft.depositor,
                    original_obligation: disbursed,
                    paid_at_start: Amount(0),
                    adjusted_obligation: disbursed,
                    obligation_status: ObligationStatus::Active,
                    released: false,
                });

                let matured_by_amount = amount.0 >= nft.deposit_amount.0;
                let matured_by_debt = nft.debt_balance.0 >= nft.deposit_amount.0;
                if matured_by_amount || matured_by_debt {
                    nft.matured = true;
                    nft.add_debt_event(DebtEvent::LoanMatured { tx_id: *tx_id, height });
                }
                nft.add_debt_event(DebtEvent::LoanIssued { tx_id: *tx_id, amount: disbursed, currency_id: mint_currency, height });
                nft.update_value_factor();
            }
        }
        Ok(())
    }
    pub fn execute_propose_chain_max_interest_rate(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::ProposeChainMaxInterestRate { tx_id, proposer, new_rate_bps } = tx {
            if !ledger.is_maintainer(proposer) {
                return Err(ColossusError::UnauthorizedPrivilegedVoter);
            }
            if *new_rate_bps == 0 || *new_rate_bps > 10_000 {
                return Err(ColossusError::InvalidInterestRate);
            }
            if ledger.active_chain_interest_rate_proposal.is_some() {
                return Err(ColossusError::InterestRateProposalAlreadyActive);
            }
            let height = BlockHeight(ledger.blocks.len() as u64);
            let mut data = Vec::new();
            data.extend_from_slice(&proposer.0);
            data.extend_from_slice(&tx_id.0);
            data.extend_from_slice(&new_rate_bps.to_le_bytes());
            data.extend_from_slice(&height.0.to_le_bytes());
            let proposal_id = colossus_types::Hash(blake3::hash(&data).into());
            let mut votes = std::collections::BTreeMap::new();
            votes.insert(proposer.0, true);
            ledger.active_chain_interest_rate_proposal = Some(colossus_ledger::ChainInterestRateProposal {
                proposal_id,
                proposer: *proposer,
                new_rate_bps: *new_rate_bps,
                created_height: height,
                votes,
            });
            Self::try_apply_chain_max_rate_proposal(ledger);
        }
        Ok(())
    }

    pub fn execute_vote_chain_max_interest_rate(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        if let Transaction::VoteChainMaxInterestRate { voter, proposal_id, approve, .. } = tx {
            if !ledger.is_maintainer(voter) {
                return Err(ColossusError::UnauthorizedPrivilegedVoter);
            }
            let Some(active) = ledger.active_chain_interest_rate_proposal.as_mut() else {
                return Err(ColossusError::InterestRateProposalNotFound);
            };
            if active.proposal_id != *proposal_id {
                return Err(ColossusError::InterestRateProposalNotFound);
            }
            active.votes.insert(voter.0, *approve);
            Self::try_apply_chain_max_rate_proposal(ledger);
        }
        Ok(())
    }

    pub fn execute_tx(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> Result<()> {
        match tx {
            Transaction::Deposit { .. } => Self::execute_deposit(ledger, tx),
            Transaction::Loan { .. } => Self::execute_loan(ledger, tx),
            Transaction::Repay { .. } => Self::execute_repay(ledger, tx),
            Transaction::Liquidate { .. } => Self::execute_liquidate(ledger, tx),
            Transaction::LedgerTransfer { .. } => Self::execute_ledger_transfer(ledger, tx),
            Transaction::ArbBorrow { .. } => Self::execute_arb_borrow(ledger, tx),
            Transaction::DepositorWithdraw { .. } => Self::execute_depositor_withdraw(ledger, tx),
            Transaction::PlaceBountyBid { .. } => Self::execute_place_bounty_bid(ledger, tx),
            Transaction::YieldSplitFreeze { .. } => Self::execute_yield_split_freeze(ledger, tx),
            Transaction::SetMinimumYieldSplitters { .. } => Self::execute_set_minimum_yield_splitters(ledger, tx),
            Transaction::YieldSplitVote { .. } => Self::execute_yield_split_vote(ledger, tx),
            Transaction::LedgerStarterSale { .. } => Self::execute_ledger_starter_sale(ledger, tx),
            Transaction::ProposeChainMaxInterestRate { .. } => Self::execute_propose_chain_max_interest_rate(ledger, tx),
            Transaction::VoteChainMaxInterestRate { .. } => Self::execute_vote_chain_max_interest_rate(ledger, tx),
        }
    }
}

pub fn process_bounty_expiry<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) {
    let height = height.into();
    let ids: Vec<BountyId> = ledger.bounties.iter().filter(|(_, b)| b.end_height == height).map(|(id, _)| *id).collect();
    for id in ids {
        let highest = ledger.bounty_auctions.get(&id).and_then(|a| a.highest.clone());
        if let Some(h) = highest {
            // settle
            if let Some(bounty) = ledger.bounties.get(&id) {
                let currency_id = bounty.currency_id;
                let current_amount = bounty.current_amount;
                let avg_interest = bounty.avg_interest;
                let reward_wallet = bounty.reward_wallet;
                let winner = h.bidder;
                // winner receives current_amount of bounty currency
                {
                    let entry = ledger.assets.entry(currency_id).or_insert(Amount(0));
                    entry.0 = entry.0.saturating_add(current_amount.0);
                }
                {
                    let acc = ledger.state.entry(winner).or_insert_with(|| colossus_ledger::AccountState {
                        address: winner, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                    });
                    acc.balances.entry(currency_id).and_modify(|e| e.0 += current_amount.0).or_insert(current_amount);
                }
                // winner pays PAI (interest + avg_interest) to reward_wallet
                let pai_id = AssetId([1u8; 32]);
                let pay_amount = Amount(avg_interest.0); // interest + avg_interest; use avg_interest as interest component
                // transfer from winner to reward wallet
                {
                    if let Some(acc) = ledger.state.get_mut(&winner) {
                        if let Some(bal) = acc.balances.get_mut(&pai_id) {
                            bal.0 = bal.0.saturating_sub(pay_amount.0);
                        }
                    }
                }
                {
                    let acc = ledger.state.entry(reward_wallet).or_insert_with(|| colossus_ledger::AccountState {
                        address: reward_wallet, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                    });
                    acc.balances.entry(pai_id).and_modify(|e| e.0 += pay_amount.0).or_insert(pay_amount);
                }
                if let Some(b) = ledger.bounties.get_mut(&id) {
                    b.winner = Some(winner);
                    b.winning_bid = Some(h.amount);
                    b.status = colossus_debt_ledger::BountyStatus::Settled;
                }
                let _ = currency_id;
            }
        } else {
            // no bids
            let (original_amount, currency_id, reward_wallet) = {
                if let Some(b) = ledger.bounties.get(&id) {
                    (b.original_amount, b.currency_id, b.reward_wallet)
                } else { (Amount(0), AssetId([0u8;32]), Address::zero()) }
            };
            let dist_total = Amount(original_amount.0 * 20 / 100);
            let wallets: Vec<Address> = ledger.state.keys().cloned().collect();
            let n = wallets.len().max(1) as u128;
            let per = Amount(dist_total.0 / n);
            let pai_id = AssetId([1u8; 32]);
            for w in wallets {
                let acc = ledger.state.entry(w).or_insert_with(|| colossus_ledger::AccountState {
                    address: w, balances: std::collections::HashMap::new(), nonce: 0, bond: Amount(0), bonded_role: None,
                });
                acc.balances.entry(pai_id).and_modify(|e| e.0 += per.0).or_insert(per);
            }
            {
                let entry = ledger.assets.entry(pai_id).or_insert(Amount(0));
                entry.0 = entry.0.saturating_add(dist_total.0);
            }
            let burn_remaining = Amount(original_amount.0.saturating_sub(dist_total.0));
            if let Some(b) = ledger.bounties.get_mut(&id) {
                b.burn_remaining = burn_remaining;
                b.status = colossus_debt_ledger::BountyStatus::ExpiredNoBids;
            }
            ledger.adjust_lender_score(reward_wallet, LENDER_FAILED_BOUNTY_DELTA);
            let _ = currency_id;
        }
    }
}

pub fn process_timed_burn<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, _height: H) {
    let ids: Vec<BountyId> = ledger.bounties.iter().filter(|(_, b)| b.status == colossus_debt_ledger::BountyStatus::ExpiredNoBids && b.burn_remaining.0 > 0).map(|(id, _)| *id).collect();
    for id in ids {
        let (currency_id, burn_per_block) = {
            let b = ledger.bounties.get(&id).unwrap();
            (b.currency_id, b.burn_per_block)
        };
        let burn_amt = {
            let b = ledger.bounties.get(&id).unwrap();
            std::cmp::min(burn_per_block.0, b.burn_remaining.0)
        };
        if burn_amt > 0 {
            {
                let entry = ledger.assets.entry(currency_id).or_insert(Amount(0));
                entry.0 = entry.0.saturating_sub(burn_amt);
            }
            if let Some(b) = ledger.bounties.get_mut(&id) {
                b.burn_remaining.0 = b.burn_remaining.0.saturating_sub(burn_amt);
                if b.burn_remaining.0 == 0 {
                    b.status = colossus_debt_ledger::BountyStatus::Burned;
                }
            }
        }
    }
}

// aliases
pub fn process_bounty_burn<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) { process_timed_burn(ledger, height); }
pub fn process_burn<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) { process_timed_burn(ledger, height); }
pub fn process_bounties<H: Into<BlockHeight>>(ledger: &mut SharedReplicatedLedger, height: H) { let height: BlockHeight = height.into(); process_bounty_expiry(ledger, height); process_timed_burn(ledger, height); }

#[cfg(test)]
mod tests {
    use super::*;
    use colossus_debt_ledger::{DebtLedger, AffectedLoan, LoanRef, ObligationStatus};
    use colossus_ledger::SharedReplicatedLedger;
    use colossus_types::{Address, Amount, AssetId, BlockHeight, LedgerId, TxId};

    #[test]
    fn bounty_created_on_withdraw() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, AssetId([3u8;32]), Amount(100), AssetId([4u8;32]), 500, BlockHeight(0));
        nft.deposit_interest_accrued = Amount(10);
        nft.affected_loans.push(AffectedLoan {
            loan_ref: LoanRef { loan_id: LedgerId([5u8;32]), loaner: Address([6u8;32]) },
            loaner: Address([6u8;32]),
            original_obligation: Amount(50),
            paid_at_start: Amount(0),
            adjusted_obligation: Amount(0),
            obligation_status: ObligationStatus::Active,
            released: false,
        });
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::DepositorWithdraw { tx_id: TxId([7u8;32]), ledger_id, depositor };
        Executor::execute_depositor_withdraw(&mut ledger, &tx).unwrap();
        assert_eq!(ledger.bounties.len(), 1);
    }

    #[test]
    fn bounty_bid_and_expiry() {
        let mut ledger = SharedReplicatedLedger::default();
        let bounty_id = colossus_types::BountyId([9u8;32]);
        let currency = AssetId([4u8;32]);
        let bounty = colossus_debt_ledger::Bounty {
            id: bounty_id,
            source_ledger_id: LedgerId([1u8;32]),
            reward_wallet: Address([2u8;32]),
            currency_id: currency,
            original_amount: Amount(100),
            current_amount: Amount(100),
            created_at_height: BlockHeight(0),
            time_limit_blocks: 5,
            end_height: BlockHeight(10),
            loan_count: 1,
            consistency_score: 1.0,
            avg_interest: Amount(10),
            burn_remaining: Amount(0),
            burn_per_block: Amount(10),
            winner: None,
            winning_bid: None,
            status: colossus_debt_ledger::BountyStatus::Open,
        };
        let auction = colossus_debt_ledger::BountyAuction {
            bounty_id,
            bids: Vec::new(),
            highest: None,
            end_height: BlockHeight(10),
        };
        ledger.bounties.insert(bounty_id, bounty);
        ledger.bounty_auctions.insert(bounty_id, auction);
        // fund bidder PAI
        let bidder = Address([10u8;32]);
        let mut acc = colossus_ledger::AccountState { address: bidder, balances: std::collections::HashMap::new(), nonce:0, bond: Amount(0), bonded_role: None };
        acc.balances.insert(AssetId([1u8;32]), Amount(50));
        ledger.state.insert(bidder, acc);
        let tx = Transaction::PlaceBountyBid { tx_id: TxId([11u8;32]), bounty_id, bidder, amount: Amount(20) };
        Executor::execute_place_bounty_bid(&mut ledger, &tx).unwrap();
        assert!(ledger.bounty_auctions.get(&bounty_id).unwrap().highest.is_some());
        process_bounty_expiry(&mut ledger, BlockHeight(10));
        assert_eq!(ledger.bounties.get(&bounty_id).unwrap().status, colossus_debt_ledger::BountyStatus::Settled);
    }

    #[test]
    fn timed_burn() {
        let mut ledger = SharedReplicatedLedger::default();
        let bounty_id = colossus_types::BountyId([12u8;32]);
        let currency = AssetId([4u8;32]);
        let bounty = colossus_debt_ledger::Bounty {
            id: bounty_id,
            source_ledger_id: LedgerId([1u8;32]),
            reward_wallet: Address([2u8;32]),
            currency_id: currency,
            original_amount: Amount(100),
            current_amount: Amount(100),
            created_at_height: BlockHeight(0),
            time_limit_blocks: 5,
            end_height: BlockHeight(0),
            loan_count: 1,
            consistency_score: 1.0,
            avg_interest: Amount(10),
            burn_remaining: Amount(30),
            burn_per_block: Amount(10),
            winner: None,
            winning_bid: None,
            status: colossus_debt_ledger::BountyStatus::ExpiredNoBids,
        };
        ledger.bounties.insert(bounty_id, bounty);
        ledger.assets.insert(currency, Amount(100));
        process_timed_burn(&mut ledger, BlockHeight(1));
        assert_eq!(ledger.bounties.get(&bounty_id).unwrap().burn_remaining, Amount(20));
    }

    #[test]
    fn partial_repay_increases_taker_score() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let borrower = Address([2u8;32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), borrower, AssetId([3u8;32]), Amount(100), AssetId([4u8;32]), 500, BlockHeight(0));
        nft.debt_balance = Amount(100);
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Repay { tx_id: TxId([7u8;32]), borrower, ledger_id, amount: Amount(30) };
        Executor::execute_repay(&mut ledger, &tx).unwrap();
        assert_eq!(ledger.taker_score(borrower), TAKER_PARTIAL_DELTA);
    }

    #[test]
    fn full_repay_increases_taker_score_more() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let borrower = Address([2u8;32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), borrower, AssetId([3u8;32]), Amount(100), AssetId([4u8;32]), 500, BlockHeight(0));
        nft.debt_balance = Amount(100);
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Repay { tx_id: TxId([7u8;32]), borrower, ledger_id, amount: Amount(100) };
        Executor::execute_repay(&mut ledger, &tx).unwrap();
        assert_eq!(ledger.taker_score(borrower), TAKER_FULL_BONUS);
    }

    #[test]
    fn withdraw_negligible_increases_lender_score() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, AssetId([3u8;32]), Amount(100), AssetId([4u8;32]), 500, BlockHeight(0));
        nft.deposit_interest_accrued = Amount(10);
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::DepositorWithdraw { tx_id: TxId([7u8;32]), ledger_id, depositor };
        Executor::execute_depositor_withdraw(&mut ledger, &tx).unwrap();
        assert_eq!(ledger.lender_score(depositor), LENDER_MATURE_LOST_LOANABILITY_DELTA);
    }

    #[test]
    fn early_withdraw_significant_decreases_lender_score() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, AssetId([3u8;32]), Amount(100), AssetId([4u8;32]), 500, BlockHeight(0));
        nft.deposit_interest_accrued = Amount(10);
        nft.created_at_height = BlockHeight(0);
        nft.affected_loans.push(AffectedLoan {
            loan_ref: LoanRef { loan_id: LedgerId([5u8;32]), loaner: Address([6u8;32]) },
            loaner: Address([6u8;32]),
            original_obligation: Amount(40_000_000_000_000_000),
            paid_at_start: Amount(0),
            adjusted_obligation: Amount(0),
            obligation_status: ObligationStatus::Active,
            released: false,
        });
        ledger.ledgers.insert(ledger_id, nft);
        ledger.adjust_lender_score(depositor, 20);
        let tx = Transaction::DepositorWithdraw { tx_id: TxId([7u8;32]), ledger_id, depositor };
        Executor::execute_depositor_withdraw(&mut ledger, &tx).unwrap();
        assert_eq!(ledger.lender_score(depositor), 15);
    }

    #[test]
    fn expired_bounty_no_bids_decreases_lender_score() {
        let mut ledger = SharedReplicatedLedger::default();
        let bounty_id = colossus_types::BountyId([9u8;32]);
        let currency = AssetId([4u8;32]);
        let reward_wallet = Address([2u8;32]);
        let bounty = colossus_debt_ledger::Bounty {
            id: bounty_id,
            source_ledger_id: LedgerId([1u8;32]),
            reward_wallet,
            currency_id: currency,
            original_amount: Amount(100),
            current_amount: Amount(100),
            created_at_height: BlockHeight(0),
            time_limit_blocks: 5,
            end_height: BlockHeight(10),
            loan_count: 1,
            consistency_score: 1.0,
            avg_interest: Amount(10),
            burn_remaining: Amount(0),
            burn_per_block: Amount(10),
            winner: None,
            winning_bid: None,
            status: colossus_debt_ledger::BountyStatus::Open,
        };
        let auction = colossus_debt_ledger::BountyAuction {
            bounty_id,
            bids: Vec::new(),
            highest: None,
            end_height: BlockHeight(10),
        };
        ledger.bounties.insert(bounty_id, bounty);
        ledger.bounty_auctions.insert(bounty_id, auction);
        ledger.adjust_lender_score(reward_wallet, 20);
        process_bounty_expiry(&mut ledger, BlockHeight(10));
        assert_eq!(ledger.lender_score(reward_wallet), 10);
    }

    #[test]
    fn ledger_transfer_resets_loanability() {
        let mut ledger = SharedReplicatedLedger::default();
        // add a second maintainer so transfer target can be in registry
        ledger.maintainer_registry.members.push(Address([9u8;32]));
        let ledger_id = LedgerId([1u8;32]);
        let owner = Address([1u8;32]);
        let depositor = Address([2u8;32]);
        let to = Address([9u8;32]);
        let mut nft = DebtLedger::new(ledger_id, owner, depositor, AssetId([3u8;32]), Amount(100), AssetId([4u8;32]), 500, BlockHeight(0));
        nft.deposit_loanability = Amount(50);
        nft.total_minted_from_ledger = Amount(50);
        nft.loan_count = 3;
        nft.update_value_factor();
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::LedgerTransfer { tx_id: TxId([7u8;32]), ledger_id, from: owner, to, pay_debts_in_one_tx: true };
        Executor::execute_ledger_transfer(&mut ledger, &tx).unwrap();
        let nft = ledger.ledgers.get(&ledger_id).unwrap();
        assert_eq!(nft.owner, to);
        assert_eq!(nft.depositor, depositor);
        assert_eq!(nft.deposit_loanability, Amount(70));
        assert_eq!(nft.total_minted_from_ledger, Amount(70));
        assert!(nft.debt_history.iter().any(|e| matches!(e, DebtEvent::LoanabilityReset { .. })));
    }

    #[test]
    fn ledger_transfer_rejects_non_registry_destination() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([11u8;32]);
        let owner = Address([1u8;32]);
        let depositor = Address([2u8;32]);
        let to = Address([99u8;32]);
        let nft = DebtLedger::new(ledger_id, owner, depositor, AssetId([3u8;32]), Amount(100), AssetId([4u8;32]), 500, BlockHeight(0));
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::LedgerTransfer { tx_id: TxId([7u8;32]), ledger_id, from: owner, to, pay_debts_in_one_tx: true };
        let res = Executor::execute_ledger_transfer(&mut ledger, &tx);
        assert!(matches!(res, Err(ColossusError::UnauthorizedMaintainerOwner)));
    }

    #[test]
    fn targeted_deposit_consistency_full() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let asset = AssetId([3u8;32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, asset, Amount(100), asset, 500, BlockHeight(0));
        nft.deposit_amount = Amount(100);
        nft.deposit_loanability = Amount(70);
        nft.interest_pool = Amount(10);
        nft.available_collateral_backing = Amount(30);
        nft.update_value_factor();
        nft.affected_loans.push(AffectedLoan {
            loan_ref: LoanRef { loan_id: LedgerId([5u8;32]), loaner: Address([6u8;32]) },
            loaner: Address([6u8;32]),
            original_obligation: Amount(50),
            paid_at_start: Amount(10),
            adjusted_obligation: Amount(0),
            obligation_status: ObligationStatus::Active,
            released: false,
        });
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Deposit { tx_id: TxId([7u8;32]), from: depositor, asset_id: asset, amount: Amount(50), bridge_proof: vec![], target_ledger_id: Some(ledger_id) };
        Executor::execute_deposit(&mut ledger, &tx).unwrap();
        let nft = ledger.ledgers.get(&ledger_id).unwrap();
        // bonus = 50*10/100 * 1.0 = 5
        assert_eq!(nft.deposit_interest_accrued, Amount(5));
        assert!(nft.debt_history.iter().any(|e| matches!(e, DebtEvent::DepositConsistencyInterest { .. })));
    }

    #[test]
    fn targeted_deposit_consistency_partial() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let asset = AssetId([3u8;32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, asset, Amount(100), asset, 500, BlockHeight(0));
        nft.deposit_amount = Amount(100);
        nft.update_value_factor();
        nft.affected_loans.push(AffectedLoan {
            loan_ref: LoanRef { loan_id: LedgerId([5u8;32]), loaner: Address([6u8;32]) },
            loaner: Address([6u8;32]),
            original_obligation: Amount(50),
            paid_at_start: Amount(10),
            adjusted_obligation: Amount(0),
            obligation_status: ObligationStatus::Active,
            released: false,
        });
        nft.affected_loans.push(AffectedLoan {
            loan_ref: LoanRef { loan_id: LedgerId([8u8;32]), loaner: Address([9u8;32]) },
            loaner: Address([9u8;32]),
            original_obligation: Amount(50),
            paid_at_start: Amount(0),
            adjusted_obligation: Amount(0),
            obligation_status: ObligationStatus::Active,
            released: false,
        });
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Deposit { tx_id: TxId([7u8;32]), from: depositor, asset_id: asset, amount: Amount(100), bridge_proof: vec![], target_ledger_id: Some(ledger_id) };
        Executor::execute_deposit(&mut ledger, &tx).unwrap();
        let nft = ledger.ledgers.get(&ledger_id).unwrap();
        // interest_add = 100*10/100=10; score=0.5; bonus=5
        assert_eq!(nft.deposit_interest_accrued, Amount(5));
    }

    #[test]
    fn targeted_deposit_consistency_no_history_cumulative() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let asset = AssetId([3u8;32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, asset, Amount(100), asset, 500, BlockHeight(0));
        nft.deposit_amount = Amount(100);
        nft.cumulative_repaid = Amount(30);
        nft.debt_balance = Amount(70);
        nft.update_value_factor();
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Deposit { tx_id: TxId([7u8;32]), from: depositor, asset_id: asset, amount: Amount(100), bridge_proof: vec![], target_ledger_id: Some(ledger_id) };
        Executor::execute_deposit(&mut ledger, &tx).unwrap();
        let nft = ledger.ledgers.get(&ledger_id).unwrap();
        // score = 30/(30+70)=0.3; interest_add=10; bonus=3
        assert_eq!(nft.deposit_interest_accrued, Amount(3));
    }

    #[test]
    fn targeted_deposit_consistency_none() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let asset = AssetId([3u8;32]);
        let nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, asset, Amount(100), asset, 500, BlockHeight(0));
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Deposit { tx_id: TxId([7u8;32]), from: depositor, asset_id: asset, amount: Amount(50), bridge_proof: vec![], target_ledger_id: Some(ledger_id) };
        Executor::execute_deposit(&mut ledger, &tx).unwrap();
        let nft = ledger.ledgers.get(&ledger_id).unwrap();
        assert_eq!(nft.deposit_interest_accrued, Amount(0));
    }

    #[test]
    fn targeted_deposit_success() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let asset = AssetId([3u8;32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, asset, Amount(100), asset, 500, BlockHeight(0));
        nft.loan_count = 2;
        nft.deposit_amount = Amount(100);
        nft.total_minted_from_ledger = Amount(70);
        nft.deposit_loanability = Amount(70);
        nft.interest_pool = Amount(10);
        nft.available_collateral_backing = Amount(30);
        nft.update_value_factor();
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Deposit { tx_id: TxId([7u8;32]), from: depositor, asset_id: asset, amount: Amount(50), bridge_proof: vec![], target_ledger_id: Some(ledger_id) };
        Executor::execute_deposit(&mut ledger, &tx).unwrap();
        let nft = ledger.ledgers.get(&ledger_id).unwrap();
        assert_eq!(nft.deposit_amount, Amount(150));
        assert_eq!(nft.total_minted_from_ledger, Amount(100)); // 70 + 30
        assert_eq!(nft.deposit_loanability, Amount(100)); // 70 + 30
        assert_eq!(nft.interest_pool, Amount(15)); // 10 + 5
        assert_eq!(nft.available_collateral_backing, Amount(50)); // 30 + 20
        assert_eq!(nft.loan_count, 2);
        assert!(nft.debt_history.iter().any(|e| matches!(e, DebtEvent::Deposit { .. })));
    }

    #[test]
    fn targeted_deposit_missing_target() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let asset = AssetId([3u8;32]);
        let tx = Transaction::Deposit { tx_id: TxId([7u8;32]), from: depositor, asset_id: asset, amount: Amount(50), bridge_proof: vec![], target_ledger_id: Some(ledger_id) };
        let res = Executor::execute_deposit(&mut ledger, &tx);
        assert!(matches!(res, Err(ColossusError::LedgerNotFound)));
    }

    #[test]
    fn targeted_deposit_frozen() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let asset = AssetId([3u8;32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, asset, Amount(100), asset, 500, BlockHeight(0));
        nft.frozen = true;
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Deposit { tx_id: TxId([7u8;32]), from: depositor, asset_id: asset, amount: Amount(50), bridge_proof: vec![], target_ledger_id: Some(ledger_id) };
        let res = Executor::execute_deposit(&mut ledger, &tx);
        assert!(matches!(res, Err(ColossusError::LedgerFrozen)));
    }

    #[test]
    fn targeted_deposit_burned() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let asset = AssetId([3u8;32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, asset, Amount(100), asset, 500, BlockHeight(0));
        nft.status = LedgerStatus::Burned;
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Deposit { tx_id: TxId([7u8;32]), from: depositor, asset_id: asset, amount: Amount(50), bridge_proof: vec![], target_ledger_id: Some(ledger_id) };
        let res = Executor::execute_deposit(&mut ledger, &tx);
        assert!(matches!(res, Err(ColossusError::CannotTransferBurnedLedger)));
    }

    #[test]
    fn targeted_deposit_depositor_mismatch() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let other = Address([9u8;32]);
        let asset = AssetId([3u8;32]);
        let nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, asset, Amount(100), asset, 500, BlockHeight(0));
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Deposit { tx_id: TxId([7u8;32]), from: other, asset_id: asset, amount: Amount(50), bridge_proof: vec![], target_ledger_id: Some(ledger_id) };
        let res = Executor::execute_deposit(&mut ledger, &tx);
        assert!(matches!(res, Err(ColossusError::InvalidTransaction)));
    }

    #[test]
    fn targeted_deposit_asset_mismatch() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([1u8;32]);
        let depositor = Address([2u8;32]);
        let asset = AssetId([3u8;32]);
        let other_asset = AssetId([4u8;32]);
        let nft = DebtLedger::new(ledger_id, Address([1u8;32]), depositor, asset, Amount(100), asset, 500, BlockHeight(0));
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Deposit { tx_id: TxId([7u8;32]), from: depositor, asset_id: other_asset, amount: Amount(50), bridge_proof: vec![], target_ledger_id: Some(ledger_id) };
        let res = Executor::execute_deposit(&mut ledger, &tx);
        assert!(matches!(res, Err(ColossusError::InvalidTransaction)));
    }

    #[test]
    fn deposit_creates_new_nft_when_no_target() {
        let mut ledger = SharedReplicatedLedger::default();
        let depositor = Address([2u8;32]);
        let asset = AssetId([3u8;32]);
        let tx_id = TxId([7u8;32]);
        let tx = Transaction::Deposit { tx_id, from: depositor, asset_id: asset, amount: Amount(50), bridge_proof: vec![], target_ledger_id: None };
        Executor::execute_deposit(&mut ledger, &tx).unwrap();
        let ledger_id = LedgerId(blake3::hash(&[depositor.0.as_slice(), tx_id.0.as_slice()].concat()).into());
        let nft = ledger.ledgers.get(&ledger_id).unwrap();
        assert_eq!(nft.depositor, depositor);
        assert!(ledger.maintainer_registry.is_maintainer(&nft.owner));
        assert_eq!(nft.owner, ledger.maintainer_registry.owner_for(&ledger_id));
    }

    #[test]
    fn loan_primary_records_without_mint() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([21u8; 32]);
        let borrower = Address([22u8; 32]);
        let asset = AssetId([23u8; 32]);
        let currency = AssetId([24u8; 32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), borrower, asset, Amount(100), asset, 500, BlockHeight(0));
        nft.deposit_loanability = Amount(70);
        nft.total_minted_from_ledger = Amount(70);
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Loan { tx_id: TxId([25u8; 32]), borrower, ledger_id, amount: Amount(50), currency_id: currency };
        Executor::execute_loan(&mut ledger, &tx).unwrap();
        let nft = ledger.ledgers.get(&ledger_id).unwrap();
        assert_eq!(nft.loan_count, 1);
        assert_eq!(nft.debt_balance, Amount(50));
        assert_eq!(nft.deposit_loanability, Amount(20));
        assert_eq!(nft.loans.len(), 1);
        assert!(ledger.assets.is_empty());
        assert!(ledger.state.get(&borrower).is_none());
        assert!(nft.debt_history.iter().any(|e| matches!(e, DebtEvent::LoanIssued { amount, currency_id: c, .. } if *amount == Amount(50) && *c == currency)));
    }

    #[test]
    fn loan_primary_rejected_exceeds_capacity() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([31u8; 32]);
        let borrower = Address([32u8; 32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), borrower, AssetId([33u8; 32]), Amount(100), AssetId([34u8; 32]), 500, BlockHeight(0));
        nft.deposit_loanability = Amount(70);
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Loan { tx_id: TxId([35u8; 32]), borrower, ledger_id, amount: Amount(71), currency_id: AssetId([36u8; 32]) };
        let res = Executor::execute_loan(&mut ledger, &tx);
        assert!(matches!(res, Err(ColossusError::InvalidTransaction)));
    }

    #[test]
    fn loan_secondary_uses_amount_minus_interest_and_no_mint() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([41u8; 32]);
        let borrower = Address([42u8; 32]);
        let deposit_currency = AssetId([43u8; 32]);
        let other_currency = AssetId([44u8; 32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), borrower, AssetId([45u8; 32]), Amount(1000), deposit_currency, 500, BlockHeight(0));
        nft.loan_count = 1;
        nft.debt_balance = Amount(100);
        nft.deposit_loanability = Amount(500);
        nft.total_minted_from_ledger = Amount(100);
        ledger.ledgers.insert(ledger_id, nft);
        // interest on 100 at 500 bps = 5, disbursed = 95
        let tx = Transaction::Loan { tx_id: TxId([46u8; 32]), borrower, ledger_id, amount: Amount(100), currency_id: other_currency };
        Executor::execute_loan(&mut ledger, &tx).unwrap();
        let nft = ledger.ledgers.get(&ledger_id).unwrap();
        assert_eq!(nft.loan_count, 2);
        assert_eq!(nft.debt_balance, Amount(195));
        assert_eq!(nft.deposit_loanability, Amount(405));
        assert_eq!(nft.loans.len(), 1);
        assert!(ledger.assets.is_empty());
        assert!(ledger.state.get(&borrower).is_none());
        assert!(nft.debt_history.iter().any(|e| matches!(e, DebtEvent::LoanIssued { amount, currency_id: c, .. } if *amount == Amount(95) && *c == deposit_currency)));
    }

    #[test]
    fn loan_secondary_rejected_when_disbursed_exceeds_capacity() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([51u8; 32]);
        let borrower = Address([52u8; 32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), borrower, AssetId([53u8; 32]), Amount(1000), AssetId([54u8; 32]), 500, BlockHeight(0));
        nft.loan_count = 1;
        nft.debt_balance = Amount(100);
        nft.deposit_loanability = Amount(50); // disbursed 95 > 50
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Loan { tx_id: TxId([55u8; 32]), borrower, ledger_id, amount: Amount(100), currency_id: AssetId([56u8; 32]) };
        let res = Executor::execute_loan(&mut ledger, &tx);
        assert!(matches!(res, Err(ColossusError::InvalidTransaction)));
    }

    #[test]
    fn loan_entire_deposit_sets_matured() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([61u8; 32]);
        let borrower = Address([62u8; 32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), borrower, AssetId([63u8; 32]), Amount(100), AssetId([64u8; 32]), 500, BlockHeight(0));
        nft.deposit_loanability = Amount(100);
        nft.total_minted_from_ledger = Amount(100);
        ledger.ledgers.insert(ledger_id, nft);
        let tx = Transaction::Loan { tx_id: TxId([65u8; 32]), borrower, ledger_id, amount: Amount(100), currency_id: AssetId([66u8; 32]) };
        Executor::execute_loan(&mut ledger, &tx).unwrap();
        let nft = ledger.ledgers.get(&ledger_id).unwrap();
        assert!(nft.matured);
        assert!(nft.debt_history.iter().any(|e| matches!(e, DebtEvent::LoanMatured { .. })));
        let tx2 = Transaction::Loan { tx_id: TxId([67u8; 32]), borrower, ledger_id, amount: Amount(1), currency_id: AssetId([66u8; 32]) };
        assert!(matches!(Executor::execute_loan(&mut ledger, &tx2), Err(ColossusError::InvalidTransaction)));
    }

    #[test]
    fn loan_rejected_when_interest_exceeds_amount() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([71u8; 32]);
        let borrower = Address([72u8; 32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), borrower, AssetId([73u8; 32]), Amount(1000), AssetId([74u8; 32]), 20000, BlockHeight(0));
        nft.loan_count = 1;
        nft.debt_balance = Amount(100);
        nft.deposit_loanability = Amount(500);
        ledger.ledgers.insert(ledger_id, nft);
        // interest = 100 * 20000 / 10000 = 200, disbursed 0
        let tx = Transaction::Loan { tx_id: TxId([75u8; 32]), borrower, ledger_id, amount: Amount(100), currency_id: AssetId([76u8; 32]) };
        assert!(matches!(Executor::execute_loan(&mut ledger, &tx), Err(ColossusError::InvalidTransaction)));
    }

    #[test]
    fn loan_rejected_missing_frozen_burned() {
        let mut ledger = SharedReplicatedLedger::default();
        let ledger_id = LedgerId([81u8; 32]);
        let borrower = Address([82u8; 32]);
        let tx = Transaction::Loan { tx_id: TxId([83u8; 32]), borrower, ledger_id, amount: Amount(10), currency_id: AssetId([84u8; 32]) };
        assert!(matches!(Executor::execute_loan(&mut ledger, &tx), Err(ColossusError::LedgerNotFound)));

        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), borrower, AssetId([85u8; 32]), Amount(100), AssetId([86u8; 32]), 500, BlockHeight(0));
        nft.frozen = true;
        ledger.ledgers.insert(ledger_id, nft);
        assert!(matches!(Executor::execute_loan(&mut ledger, &tx), Err(ColossusError::LedgerFrozen)));

        ledger.ledgers.get_mut(&ledger_id).unwrap().frozen = false;
        ledger.ledgers.get_mut(&ledger_id).unwrap().status = LedgerStatus::Burned;
        assert!(matches!(Executor::execute_loan(&mut ledger, &tx), Err(ColossusError::CannotTransferBurnedLedger)));
    }

    #[test]
    fn new_ledger_uses_chain_max_interest_rate() {
        let mut ledger = SharedReplicatedLedger::default();
        ledger.chain_max_interest_rate_bps = 750;
        let depositor = Address([9u8; 32]);
        let tx = Transaction::Deposit { tx_id: TxId([7u8; 32]), from: depositor, asset_id: AssetId([8u8; 32]), amount: Amount(100), bridge_proof: vec![], target_ledger_id: None };
        Executor::execute_deposit(&mut ledger, &tx).unwrap();
        let nft = ledger.ledgers.values().next().unwrap();
        assert_eq!(nft.interest_rate_bps, 750);
    }

    #[test]
    fn loan_interest_capped_by_chain_max_for_existing_ledger() {
        let mut ledger = SharedReplicatedLedger::default();
        ledger.chain_max_interest_rate_bps = 100; // 1%
        let ledger_id = LedgerId([91u8; 32]);
        let borrower = Address([92u8; 32]);
        let mut nft = DebtLedger::new(ledger_id, Address([1u8;32]), borrower, AssetId([93u8; 32]), Amount(1000), AssetId([94u8; 32]), 2000, BlockHeight(0));
        nft.loan_count = 1;
        nft.debt_balance = Amount(100);
        nft.deposit_loanability = Amount(500);
        nft.total_minted_from_ledger = Amount(100);
        ledger.ledgers.insert(ledger_id, nft);
        // secondary loan interest should use min(2000,100)=100 bps => 100 * 100 / 10000 = 1
        let tx = Transaction::Loan { tx_id: TxId([95u8; 32]), borrower, ledger_id, amount: Amount(100), currency_id: AssetId([94u8; 32]) };
        Executor::execute_loan(&mut ledger, &tx).unwrap();
        let nft = ledger.ledgers.get(&ledger_id).unwrap();
        assert_eq!(nft.debt_balance.0, 199); // 100 existing + 99 disbursed
        assert_eq!(nft.loans.last().unwrap().amount.0, 99);
        assert_eq!(nft.loans.last().unwrap().interest_rate_bps, 100);
    }

    #[test]
    fn chain_max_rate_governance_requires_privileged_and_majority() {
        let mut ledger = SharedReplicatedLedger::default();
        let m1 = Address([1u8; 32]);
        let mut m2b = [0u8; 32]; m2b[0] = 2;
        let mut m3b = [0u8; 32]; m3b[0] = 3;
        let m2 = Address(m2b);
        let m3 = Address(m3b);
        ledger.maintainer_registry.members = vec![m1, m2, m3];
        let non = Address([9u8; 32]);
        let tx = Transaction::ProposeChainMaxInterestRate { tx_id: TxId([11u8; 32]), proposer: non, new_rate_bps: 700 };
        assert!(matches!(Executor::execute_propose_chain_max_interest_rate(&mut ledger, &tx), Err(ColossusError::UnauthorizedPrivilegedVoter)));

        let tx = Transaction::ProposeChainMaxInterestRate { tx_id: TxId([12u8; 32]), proposer: m1, new_rate_bps: 700 };
        Executor::execute_tx(&mut ledger, &tx).unwrap();
        assert!(ledger.active_chain_interest_rate_proposal.is_some());
        assert_eq!(ledger.chain_max_interest_rate_bps, 500);
        let proposal_id = ledger.active_chain_interest_rate_proposal.as_ref().unwrap().proposal_id;

        let tx = Transaction::VoteChainMaxInterestRate { tx_id: TxId([13u8; 32]), voter: non, proposal_id, approve: true };
        assert!(matches!(Executor::execute_tx(&mut ledger, &tx), Err(ColossusError::UnauthorizedPrivilegedVoter)));

        let tx = Transaction::VoteChainMaxInterestRate { tx_id: TxId([14u8; 32]), voter: m2, proposal_id, approve: true };
        Executor::execute_tx(&mut ledger, &tx).unwrap();
        assert_eq!(ledger.chain_max_interest_rate_bps, 700);
        assert!(ledger.active_chain_interest_rate_proposal.is_none());
    }

    #[test]
    fn chain_max_rate_proposal_rejected_when_active() {
        let mut ledger = SharedReplicatedLedger::default();
        let m1 = Address([1u8; 32]);
        let mut m2b = [0u8; 32]; m2b[0] = 2;
        let mut m3b = [0u8; 32]; m3b[0] = 3;
        ledger.maintainer_registry.members = vec![m1, Address(m2b), Address(m3b)];
        let tx = Transaction::ProposeChainMaxInterestRate { tx_id: TxId([21u8; 32]), proposer: m1, new_rate_bps: 600 };
        Executor::execute_tx(&mut ledger, &tx).unwrap();
        let tx = Transaction::ProposeChainMaxInterestRate { tx_id: TxId([22u8; 32]), proposer: m1, new_rate_bps: 800 };
        assert!(matches!(Executor::execute_tx(&mut ledger, &tx), Err(ColossusError::InterestRateProposalAlreadyActive)));
    }

}


pub fn execute_place_bounty_bid(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> colossus_types::Result<()> {
    Executor::execute_place_bounty_bid(ledger, tx)
}

pub fn execute_yield_split_freeze(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> colossus_types::Result<()> {
    Executor::execute_yield_split_freeze(ledger, tx)
}
pub fn execute_set_minimum_yield_splitters(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> colossus_types::Result<()> {
    Executor::execute_set_minimum_yield_splitters(ledger, tx)
}
pub fn execute_ledger_starter_sale(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> colossus_types::Result<()> {
    Executor::execute_ledger_starter_sale(ledger, tx)
}
pub fn execute_loan(ledger: &mut SharedReplicatedLedger, tx: &Transaction) -> colossus_types::Result<()> {
    Executor::execute_loan(ledger, tx)
}
