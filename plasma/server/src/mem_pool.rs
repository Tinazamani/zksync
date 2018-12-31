use std::sync::{Arc, mpsc::{channel, Sender, Receiver}};
use plasma::models::{TransferTx, TransferBlock, Block, AccountId, Nonce};
use fnv::FnvHashMap;
use super::models::{StateProcessingRequest, AppliedTransactions, RejectedTransactions};
use super::config;
use priority_queue::PriorityQueue;
use bigdecimal::BigDecimal;
use im::ordmap::OrdMap;
use num_traits::Zero;
use std::borrow::BorrowMut;

const MAX_TRANSACTIONS_PER_ACCOUNT: usize = 128;

#[derive(Default, Debug, Clone)]
struct AccountTxQueue {
    pub queue: OrdMap<Nonce, TransferTx>,
}

pub type TxResult<T> = std::result::Result<T, String>;

impl AccountTxQueue {

    /// Returns true if new item added
    pub fn insert(&mut self, tx: TransferTx) -> bool {
        self.queue.insert(tx.nonce, tx).is_none()
    }

    fn min_nonce(&self) -> Nonce {
        self.queue.get_min().map(|(k,_)| *k).unwrap_or(0)
    }

    pub fn pending_nonce(&self) -> Nonce {
        let mut next_nonce = self.min_nonce();
        for nonce in self.queue.keys() {
            if next_nonce != *nonce { break }
            next_nonce += 1;
        }
        next_nonce
    }

    pub fn next_fee(&self) -> Option<BigDecimal> {
        self.queue.values().next().map(|v| v.fee.clone())
    }

    pub fn pop(&mut self, expected_nonce: Nonce) -> (RejectedTransactions, Option<TransferTx>) {

        let exact_match = self.min_nonce() == expected_nonce;
        let (lesser, mut tx, greater) = self.queue.split_lookup(&expected_nonce);
        let mut rejected: RejectedTransactions = lesser.into_iter().map(|(k,v)| v).collect();
        if tx.is_some() {
            self.queue = greater;
            if !exact_match {
                // put it back for this loop cycle, because we need to update priority queue first: maybe it's not the highest fee
                self.queue.insert(expected_nonce, tx.take().unwrap());
            }
        } else {
            self.queue = OrdMap::new();
            rejected.extend(greater.into_iter().map(|(k,v)| v));
        }

        (rejected, tx)
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    } 

}

#[derive(Default, Debug, Clone)]
pub struct TxQueue {
    queues: FnvHashMap<AccountId, AccountTxQueue>,
    order:  PriorityQueue<AccountId, BigDecimal>,
    len:    usize,
}

// For state_keeper::create_transfer_block()
impl TxQueue {

    pub fn peek_next(&self) -> Option<AccountId> {
        self.order.peek().map(|(&id, _)| id)
    }

    /// next() must be called immediately after peek_next(), so that the queue for account_id exists
    pub fn next(&mut self, account_id: AccountId, next_nonce: Nonce) -> (RejectedTransactions, Option<TransferTx>) {
        assert_eq!(account_id, self.peek_next().unwrap());
        let (rejected, tx, next_fee) = {
            let queue = self.queues.get_mut(&account_id).unwrap();
            let (rejected, tx) = queue.pop(next_nonce);
            let ejected = rejected.len() + if tx.is_some() {1} else {0};
            self.len -= ejected;
            (rejected, tx, queue.next_fee())
        };
        if let Some(next_fee) = next_fee {
            // update priority
            self.order.change_priority(&account_id, next_fee);
        } else {
            // remove empty queue
            self.order.pop();
            self.queues.remove(&account_id);
        }
        (rejected, tx)
    }
}

impl TxQueue {

    fn ensure_queue(&mut self, account_id: AccountId)  {
        if self.queues.get(&account_id).is_none() {
            self.queues.insert(account_id, AccountTxQueue::default());
            self.order.push(account_id, BigDecimal::zero());
        }
    }

    fn insert(&mut self, tx: TransferTx) {
        let from = tx.from;
        self.ensure_queue(from);
        let queue = self.queues.get_mut(&from).unwrap();
        if queue.insert(tx) {
            self.len += 1;
        }
        self.order.change_priority(&from, queue.next_fee().unwrap());
    }

    fn batch_insert(&mut self, list: Vec<TransferTx>) {
        // TODO: optimize performance: group by accounts, then update order once per account
        for tx in list.into_iter() {
            self.insert(tx);
        }
    }

    fn pending_nonce(&self, account_id: AccountId) -> Option<Nonce> {
        self.queues.get(&account_id).map(|queue| queue.pending_nonce())
    }

    fn len(&self) -> usize {
        self.len
    }
}


#[derive(Default)]
pub struct MemPool {
    // Batch size
    batch_requested:    bool,
    queue:              TxQueue,
}

pub enum MempoolRequest {
    AddTransaction(TransferTx),
    GetPendingNonce(AccountId, Sender<Option<Nonce>>),
    ProcessBatch,
}

impl MemPool {

    fn run(&mut self, 
        tx_for_requests: Sender<MempoolRequest>,
        rx_for_requests: Receiver<MempoolRequest>, 
        tx_for_blocks: Sender<StateProcessingRequest>) 
    {
        for req in rx_for_requests {            
            match req {
                MempoolRequest::AddTransaction(tx) => {
                    let add_result = self.add_transaction(tx);
                    if let Err(err) = add_result {
                        println!("error adding transaction to mempool: {}", err);
                        // TODO: return error message to api server
                    } else {
                        println!("mempool queue length = {}", self.queue.len());
                        // TODO: also check that batch is now possible (e.g. that Ethereum queue is not too long)
                        if !self.batch_requested && self.queue.len() >= config::TRANSFER_BATCH_SIZE {
                            println!("batch processing requested");
                            self.batch_requested = true;
                            tx_for_requests.send(MempoolRequest::ProcessBatch);
                        }
                    }
                },
                MempoolRequest::ProcessBatch => {
                    self.batch_requested = false;
                    let do_padding = false; // TODO: use when neccessary
                    self.process_batch(do_padding, &tx_for_blocks);
                },
                MempoolRequest::GetPendingNonce(account_id, channel) => {
                    channel.send(self.queue.pending_nonce(account_id));
                },
            }
        }
    }

    fn add_transaction(&mut self, transaction: TransferTx) -> TxResult<()> {
        println!("adding tx to mem pool");

        if let Some(queue) = self.queue.queues.get(&transaction.from) {
            if queue.len() >= MAX_TRANSACTIONS_PER_ACCOUNT {
                return Err(format!("Too many transactions in the queue for this account"))
            }

            // TODO: replace existing tx if fee is higher

            let pending_nonce = queue.pending_nonce();
            if transaction.nonce != pending_nonce {
                return Err(format!("Nonce is out of sequence: expected {}, got {}", pending_nonce, transaction.nonce))
            }
        }

        self.queue.insert(transaction);
        // TODO: commit to database
        Ok(())
    }

    fn process_batch(&mut self, do_padding: bool, tx_for_blocks: &Sender<StateProcessingRequest>) {

        // send request to state_keeper
        let (tx, rx) = channel();

        // move ownership of queue to the state_keeper thread 
        let queue = std::mem::replace(&mut self.queue, TxQueue::default());

        let request = StateProcessingRequest::CreateTransferBlock(queue, do_padding, tx);
        tx_for_blocks.send(request).expect("must send block processing request");

        // now wait for state_keeper to return a result
        let (queue, result) = rx.recv().expect("must receive answer for block processing request");

        // take ownership of queue back
        self.queue = queue;

        match result {
            Ok((applied, block_number)) => {
                // TODO: remove applied, block_number, wait here for committer instead
            },
            Err((valid, invalid)) => {
                println!("creating transfer block failed: {} transactions rejected, {} going back to queue", invalid.len(), valid.len());
                self.queue.batch_insert(valid)
                // TODO: remove invalid transactions from db
            },
        };
    }

}

pub fn start_mem_pool(mut mem_pool: MemPool, 
    tx_for_requests: Sender<MempoolRequest>, 
    rx_for_requests: Receiver<MempoolRequest>, 
    tx_for_blocks: Sender<StateProcessingRequest>) 
{
    std::thread::Builder::new().name("mem_pool".to_string()).spawn(move || {  
        mem_pool.run(tx_for_requests, rx_for_requests, tx_for_blocks);
    });
}


#[cfg(test)]
mod test {

    use plasma::models::*;
    use bigdecimal::BigDecimal;

    pub fn tx(from: AccountId, nonce: u32,  fee: u32) -> TransferTx {
        let mut tx = TransferTx::default();
        tx.from = from;
        tx.nonce = nonce;
        tx.fee = BigDecimal::from(fee);
        tx
    }

}

#[test] 
fn test_account_tx_queue() {

    let mut queue = AccountTxQueue::default();

    assert_eq!(queue.pending_nonce(), 0);
    assert_eq!(queue.next_fee(), None);

    assert_eq!(queue.insert(test::tx(1, 5, 20)), true);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.insert(test::tx(1, 5, 20)), false);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.next_fee().unwrap(), BigDecimal::from(20));

    assert_eq!(queue.pending_nonce(), 6);

    assert_eq!(queue.insert(test::tx(1, 7, 40)), true);
    assert_eq!(queue.len(), 2);
    assert_eq!(queue.next_fee().unwrap(), BigDecimal::from(20));

    let mut q = queue.clone();
    let (rejected, tx) = q.pop(5);
    assert_eq!(rejected.len(), 0); 
    assert_eq!(tx.unwrap().nonce, 5); 
    assert_eq!(q.len(), 1);
    assert_eq!(q.next_fee().unwrap(), BigDecimal::from(40));
    assert_eq!(q.pending_nonce(), 8);

    let mut q = queue.clone();
    let (rejected, tx) = q.pop(6);
    assert_eq!(rejected.len(), 2); 
    assert_eq!(tx.is_none(), true);
    assert_eq!(q.len(), 0);
    assert_eq!(q.next_fee(), None);

    let mut q = queue.clone();
    let (rejected, tx) = q.pop(7);
    assert_eq!(rejected.len(), 1); 
    assert_eq!(tx, None);
    assert_eq!(q.len(), 1);
    assert_eq!(q.pending_nonce(), 8);

    let mut q = queue.clone();
    let (rejected, tx) = q.pop(8);
    assert_eq!(rejected.len(), 2); 
    assert_eq!(tx.is_none(), true);
    assert_eq!(q.pending_nonce(), 0);

    let mut q = queue.clone();
    assert_eq!(q.insert(test::tx(1, 6, 40)), true);
    let (rejected, tx) = q.pop(6);
    assert_eq!(rejected.len(), 1); 
    assert_eq!(tx, None);

    let (rejected, tx) = q.pop(6);
    assert_eq!(rejected.len(), 0); 
    assert_eq!(tx.unwrap().nonce, 6);
    assert_eq!(q.len(), 1);
    assert_eq!(q.next_fee().unwrap(), BigDecimal::from(40));
    assert_eq!(q.pending_nonce(), 8);

}

#[test] 
fn test_tx_queue() {
    let mut q = TxQueue::default();
    assert_eq!(q.peek_next(), None);

    q.insert(test::tx(1, 5, 20));
    assert_eq!(q.len(), 1);
    assert_eq!(q.peek_next().unwrap(), 1);

    q.insert(test::tx(2, 0, 40));
    assert_eq!(q.len(), 2);
    assert_eq!(q.peek_next().unwrap(), 2);

    q.insert(test::tx(1, 6, 50));
    assert_eq!(q.len(), 3);
    assert_eq!(q.peek_next().unwrap(), 2);

    q.insert(test::tx(1, 5, 50));
    assert_eq!(q.len(), 3);
    assert_eq!(q.peek_next().unwrap(), 1);

    let _q = q;

    let mut q = _q.clone();
    let (rejected, tx) = q.next(1, 5);
    assert_eq!(rejected.len(), 0);
    assert_eq!(tx.as_ref().unwrap().from, 1);
    assert_eq!(tx.as_ref().unwrap().nonce, 5);
    assert_eq!(tx.as_ref().unwrap().fee, BigDecimal::from(50));
    assert_eq!(q.len(), 2);
    assert_eq!(q.peek_next().unwrap(), 1);

    let mut q = _q.clone();
    let (rejected, tx) = q.next(1, 6);
    assert_eq!(rejected.len(), 1);
    assert_eq!(tx, None);
    assert_eq!(q.len(), 2);
    assert_eq!(q.peek_next().unwrap(), 1);

    let (rejected, tx) = q.next(1, 6);
    assert_eq!(rejected.len(), 0);
    assert_eq!(tx.as_ref().unwrap().from, 1);
    assert_eq!(tx.as_ref().unwrap().nonce, 6);
    assert_eq!(tx.as_ref().unwrap().fee, BigDecimal::from(50));
    assert_eq!(q.len(), 1);
    assert_eq!(q.peek_next().unwrap(), 2);

    let (rejected, tx) = q.next(2, 0);
    assert_eq!(rejected.len(), 0);
    assert_eq!(tx.as_ref().unwrap().from, 2);
    assert_eq!(tx.as_ref().unwrap().nonce, 0);
    assert_eq!(q.len(), 0);
    assert_eq!(q.peek_next(), None);
}