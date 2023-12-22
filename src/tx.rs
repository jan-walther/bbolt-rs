use crate::bucket::{
  BucketCell, BucketIAPI, BucketImpl, BucketRW, BucketRwCell, BucketRwIAPI, BucketRwImpl,
};
use crate::common::defaults::DEFAULT_PAGE_SIZE;
use crate::common::memory::{BCell, LCell};
use crate::common::meta::Meta;
use crate::common::page::{MutPage, PageInfo, RefPage};
use crate::common::selfowned::SelfOwned;
use crate::common::{BVec, HashMap, PgId, SplitRef, TxId};
use crate::cursor::{CursorImpl, CursorRwIAPI, CursorRwImpl, InnerCursor};
use crate::db::{DBShared, Pager};
use crate::freelist::Freelist;
use crate::node::NodeRwCell;
use crate::{BucketApi, CursorApi, CursorRwApi};
use bumpalo::Bump;
use parking_lot::{RwLockReadGuard, RwLockWriteGuard};
use std::borrow::Cow;
use std::cell::{Ref, RefCell, RefMut};
use std::marker::{PhantomData, PhantomPinned};
use std::mem::MaybeUninit;
use std::ops::{AddAssign, Deref, DerefMut, Sub};
use std::pin::Pin;
use std::ptr::{addr_of, addr_of_mut};
use std::rc::Rc;
use std::time::Duration;
use std::{cell, mem};

pub trait TxApi<'tx> {
  type CursorType: CursorApi<'tx>;
  type BucketType: BucketApi<'tx>;

  /// ID returns the transaction id.
  fn id(&self) -> TxId;

  /// Size returns current database size in bytes as seen by this transaction.
  fn size(&self) -> u64;

  /// Writable returns whether the transaction can perform write operations.
  fn writeable(&self) -> bool;

  /// Cursor creates a cursor associated with the root bucket.
  /// All items in the cursor will return a nil value because all root bucket keys point to buckets.
  /// The cursor is only valid as long as the transaction is open.
  /// Do not use a cursor after the transaction is closed.
  fn cursor(&self) -> Self::CursorType;

  /// Stats retrieves a copy of the current transaction statistics.
  fn stats(&self) -> TxStats;

  /// Bucket retrieves a bucket by name.
  /// Returns nil if the bucket does not exist.
  /// The bucket instance is only valid for the lifetime of the transaction.
  fn bucket(&self, name: &[u8]) -> Option<Self::BucketType>;

  fn for_each<F: FnMut(&[u8], Self::BucketType)>(&self, f: F) -> crate::Result<()>;

  /// Rollback closes the transaction and ignores all previous updates. Read-only
  /// transactions must be rolled back and not committed.
  fn rollback(self) -> crate::Result<()>;

  /// Page returns page information for a given page number.
  /// This is only safe for concurrent use when used by a writable transaction.
  fn page(&self, id: PgId) -> crate::Result<Option<PageInfo>>;
}

pub trait TxRwApi<'tx>: TxApi<'tx> {
  type CursorRwType: CursorRwApi<'tx>;

  /// Cursor creates a cursor associated with the root bucket.
  /// All items in the cursor will return a nil value because all root bucket keys point to buckets.
  /// The cursor is only valid as long as the transaction is open.
  /// Do not use a cursor after the transaction is closed.
  fn cursor_mut(&mut self) -> Self::CursorRwType;

  /// CreateBucket creates a new bucket.
  /// Returns an error if the bucket already exists, if the bucket name is blank, or if the bucket name is too long.
  /// The bucket instance is only valid for the lifetime of the transaction.
  fn create_bucket(&mut self, name: &[u8]) -> crate::Result<Self::BucketType>;

  /// CreateBucketIfNotExists creates a new bucket if it doesn't already exist.
  /// Returns an error if the bucket name is blank, or if the bucket name is too long.
  /// The bucket instance is only valid for the lifetime of the transaction.
  fn create_bucket_if_not_exists(&mut self, name: &[u8]) -> crate::Result<Self::BucketType>;

  /// DeleteBucket deletes a bucket.
  /// Returns an error if the bucket cannot be found or if the key represents a non-bucket value.
  fn delete_bucket(&mut self, name: &[u8]) -> crate::Result<()>;

  /// Commit writes all changes to disk and updates the meta page.
  /// Returns an error if a disk write error occurs, or if Commit is
  /// called on a read-only transaction.
  fn commit(self) -> crate::Result<()>;
}

// TODO: Add functions to simplify access
#[derive(Copy, Clone, Default)]
pub struct TxStats {
  // Page statistics.
  //
  /// number of page allocations
  pub(crate) page_count: i64,
  /// total bytes allocated
  pub(crate) page_alloc: i64,

  // Cursor statistics.
  //
  /// number of cursors created
  pub(crate) cursor_count: i64,

  // Node statistics
  //
  /// number of node allocations
  pub(crate) node_count: i64,
  /// number of node dereferences
  pub(crate) node_deref: i64,

  // Rebalance statistics.
  //
  /// number of node rebalances
  pub(crate) rebalance: i64,
  /// total time spent rebalancing
  pub(crate) rebalance_time: Duration,

  // Split/Spill statistics.
  //
  /// number of nodes split
  pub(crate) split: i64,
  /// number of nodes spilled
  pub(crate) spill: i64,
  /// total time spent spilling
  pub(crate) spill_time: Duration,

  // Write statistics.
  //
  /// number of writes performed
  pub(crate) write: i64,
  /// total time spent writing to disk
  pub(crate) write_time: Duration,
}

impl AddAssign<TxStats> for TxStats {
  fn add_assign(&mut self, rhs: TxStats) {
    self.page_count += rhs.page_count;
    self.page_alloc += rhs.page_alloc;
    self.cursor_count += rhs.cursor_count;
    self.node_count += rhs.node_count;
    self.node_deref += rhs.node_deref;
    self.rebalance += rhs.rebalance;
    self.rebalance_time += rhs.rebalance_time;
    self.split += rhs.split;
    self.spill += rhs.spill;
    self.spill_time += rhs.spill_time;
    self.write += rhs.write;
    self.write_time += rhs.write_time;
  }
}

impl Sub<TxStats> for TxStats {
  type Output = TxStats;

  fn sub(self, rhs: TxStats) -> Self::Output {
    TxStats {
      page_count: self.page_count - rhs.page_count,
      page_alloc: self.page_alloc - rhs.page_alloc,
      cursor_count: self.cursor_count - rhs.cursor_count,
      node_count: self.node_count - rhs.node_count,
      node_deref: self.node_deref - rhs.node_deref,
      rebalance: self.rebalance - rhs.rebalance,
      rebalance_time: self.rebalance_time - rhs.rebalance_time,
      split: self.split - rhs.split,
      spill: self.spill - rhs.spill,
      spill_time: self.spill_time - rhs.spill_time,
      write: self.write - rhs.write,
      write_time: self.write_time - rhs.write_time,
    }
  }
}

pub(crate) trait TxIAPI<'tx>: SplitRef<TxR<'tx>, Self::BucketType, TxW<'tx>> {
  type BucketType: BucketIAPI<'tx, Self>;

  fn bump(self) -> &'tx Bump {
    self.split_r().b
  }

  fn page_size(self) -> usize {
    self.split_r().page_size
  }

  fn meta<'a>(&'a self) -> Ref<'a, Meta>
  where
    'tx: 'a,
  {
    Ref::map(self.split_r(), |tx| &tx.meta)
  }

  fn page(self, id: PgId) -> RefPage<'tx> {
    self.split_r().pager.page(id)
  }

  /// See [TxApi::id]
  fn api_id(self) -> TxId {
    self.split_r().meta.txid()
  }

  /// See [TxApi::size]
  fn api_size(self) -> u64 {
    let r = self.split_r();
    r.meta.pgid().0 * r.meta.page_size() as u64
  }

  /// See [TxApi::writeable]
  fn api_writeable(self) -> bool {
    self.split_ow().is_some()
  }

  /// See [TxApi::cursor]
  fn api_cursor(self) -> InnerCursor<'tx, Self, Self::BucketType> {
    let root_bucket = self.root_bucket();
    root_bucket.i_cursor()
  }

  /// See [TxApi::stats]
  fn api_stats(self) -> TxStats {
    self.split_r().stats
  }

  fn mut_stats<'a>(&'a self) -> RefMut<'a, TxStats>
  where
    'tx: 'a,
  {
    RefMut::map(self.split_r_mut(), |r| &mut r.stats)
  }

  fn root_bucket(self) -> Self::BucketType {
    self.split_bound()
  }

  /// See [TxApi::bucket]
  fn api_bucket(self, name: &[u8]) -> Option<Self::BucketType> {
    let root_bucket = self.root_bucket();
    root_bucket.api_bucket(name)
  }

  /// See [TxApi::for_each]
  fn api_for_each<F: FnMut(&[u8], Self::BucketType)>(&self, mut f: F) -> crate::Result<()> {
    let root_bucket = self.root_bucket();
    // TODO: Are we calling the right function?
    root_bucket.api_for_each_bucket(|k| {
      let bucket = root_bucket.api_bucket(k).unwrap();
      Ok(f(k, bucket))
    })
  }

  /// See [TxApi::rollback]
  fn api_rollback(self) -> crate::Result<()> {
    todo!()
  }

  fn non_physical_rollback(self) -> crate::Result<()>;

  fn rollback(self) -> crate::Result<()> {
    self.split_r_mut().is_rollback = true;
    Ok(())
  }

  /// See [TxApi::page]
  fn api_page(&self, id: PgId) -> crate::Result<Option<PageInfo>> {
    let r = self.split_r();
    if id >= r.meta.pgid() {
      return Ok(None);
    }
    //TODO: Check if freelist loaded
    //WHEN: Freelists can be unloaded

    let p = r.pager.page(id);
    let id = p.id;
    let count = p.count as u64;
    let overflow_count = p.overflow as u64;

    let t = if r.pager.page_is_free(id) {
      Cow::Borrowed("free")
    } else {
      p.page_type()
    };
    let info = PageInfo {
      id: id.0,
      t,
      count,
      overflow_count,
    };
    Ok(Some(info))
  }

  fn close(self) -> crate::Result<()>;
}

pub(crate) trait TxRwIAPI<'tx>: TxIAPI<'tx> {
  type CursorRwType: CursorRwIAPI<'tx>;
  fn freelist(self) -> RefMut<'tx, Freelist>;

  fn allocate(self, count: usize) -> crate::Result<MutPage<'tx>>;

  /// See [TxRwApi::cursor_mut]
  fn api_cursor_mut(self) -> Self::CursorRwType;

  /// See [TxRwApi::create_bucket]
  fn api_create_bucket(self, name: &[u8]) -> crate::Result<Self::BucketType>;

  /// See [TxRwApi::create_bucket_if_not_exists]
  fn api_create_bucket_if_not_exist(self, name: &[u8]) -> crate::Result<Self::BucketType>;

  /// See [TxRwApi::delete_bucket]
  fn api_delete_bucket(self, name: &[u8]) -> crate::Result<()>;

  /// See [TxRwApi::commit]
  fn api_commit(self) -> crate::Result<()>;
}

pub(crate) struct TxImplTODORenameMe {}

impl TxImplTODORenameMe {
  pub(crate) fn for_each_page<'tx, T: TxIAPI<'tx>, F: FnMut(&RefPage, usize, &[PgId])>(
    cell: &T, root: PgId, f: F,
  ) {
    todo!()
  }
}

pub struct TxR<'tx> {
  b: &'tx Bump,
  page_size: usize,
  pager: &'tx dyn Pager,
  pub(crate) stats: TxStats,
  meta: Meta,
  is_rollback: bool,
  p: PhantomData<&'tx u8>,
}

pub struct TxW<'tx> {
  pages: HashMap<'tx, PgId, MutPage<'tx>>,
  //TODO: We leak memory when this drops. Need special handling here
  commit_handlers: BVec<'tx, Box<dyn FnMut()>>,
  p: PhantomData<&'tx u8>,
}

pub struct TxRW<'tx> {
  r: TxR<'tx>,
  w: TxW<'tx>,
}

#[derive(Copy, Clone)]
pub struct TxCell<'tx> {
  pub(crate) cell: BCell<'tx, TxR<'tx>, BucketCell<'tx>>,
}

impl<'tx> SplitRef<TxR<'tx>, BucketCell<'tx>, TxW<'tx>> for TxCell<'tx> {
  fn split_r(&self) -> Ref<TxR<'tx>> {
    self.cell.0.borrow()
  }

  fn split_r_ow(&self) -> (Ref<TxR<'tx>>, Option<Ref<TxW<'tx>>>) {
    (self.cell.0.borrow(), None)
  }

  fn split_ow(&self) -> Option<Ref<TxW<'tx>>> {
    None
  }

  fn split_bound(&self) -> BucketCell<'tx> {
    self.cell.1
  }

  fn split_ref(&self) -> (Ref<TxR<'tx>>, BucketCell<'tx>, Option<Ref<TxW<'tx>>>) {
    (self.cell.0.borrow(), self.cell.1, None)
  }

  fn split_r_mut(&self) -> RefMut<TxR<'tx>> {
    self.cell.0.borrow_mut()
  }

  fn split_r_ow_mut(&self) -> (RefMut<TxR<'tx>>, Option<RefMut<TxW<'tx>>>) {
    (self.cell.0.borrow_mut(), None)
  }

  fn split_ow_mut(&self) -> Option<RefMut<TxW<'tx>>> {
    None
  }

  fn split_ref_mut(&self) -> (RefMut<TxR<'tx>>, BucketCell<'tx>, Option<RefMut<TxW<'tx>>>) {
    (self.cell.0.borrow_mut(), self.cell.1, None)
  }
}

impl<'tx> TxIAPI<'tx> for TxCell<'tx> {
  type BucketType = BucketCell<'tx>;

  fn non_physical_rollback(self) -> crate::Result<()> {
    todo!()
  }

  fn close(self) -> crate::Result<()> {
    todo!()
  }
}

#[derive(Copy, Clone)]
pub struct TxRwCell<'tx> {
  pub(crate) cell: BCell<'tx, TxRW<'tx>, BucketRwCell<'tx>>,
}

impl<'tx> SplitRef<TxR<'tx>, BucketRwCell<'tx>, TxW<'tx>> for TxRwCell<'tx> {
  fn split_r(&self) -> Ref<TxR<'tx>> {
    Ref::map(self.cell.0.borrow(), |c| &c.r)
  }

  fn split_r_ow(&self) -> (Ref<TxR<'tx>>, Option<Ref<TxW<'tx>>>) {
    let (r, w) = Ref::map_split(self.cell.0.borrow(), |b| (&b.r, &b.w));
    (r, Some(w))
  }

  fn split_ow(&self) -> Option<Ref<TxW<'tx>>> {
    Some(Ref::map(self.cell.0.borrow(), |c| &c.w))
  }

  fn split_bound(&self) -> BucketRwCell<'tx> {
    self.cell.1
  }

  fn split_ref(&self) -> (Ref<TxR<'tx>>, BucketRwCell<'tx>, Option<Ref<TxW<'tx>>>) {
    let (r, w) = Ref::map_split(self.cell.0.borrow(), |b| (&b.r, &b.w));
    (r, self.cell.1, Some(w))
  }

  fn split_r_mut(&self) -> RefMut<TxR<'tx>> {
    RefMut::map(self.cell.0.borrow_mut(), |c| &mut c.r)
  }

  fn split_r_ow_mut(&self) -> (RefMut<TxR<'tx>>, Option<RefMut<TxW<'tx>>>) {
    let (r, w) = RefMut::map_split(self.cell.0.borrow_mut(), |b| (&mut b.r, &mut b.w));
    (r, Some(w))
  }

  fn split_ow_mut(&self) -> Option<RefMut<TxW<'tx>>> {
    Some(RefMut::map(self.cell.0.borrow_mut(), |c| &mut c.w))
  }

  fn split_ref_mut(
    &self,
  ) -> (
    RefMut<TxR<'tx>>,
    BucketRwCell<'tx>,
    Option<RefMut<TxW<'tx>>>,
  ) {
    let (r, w) = RefMut::map_split(self.cell.0.borrow_mut(), |b| (&mut b.r, &mut b.w));
    (r, self.cell.1, Some(w))
  }
}

impl<'tx> TxIAPI<'tx> for TxRwCell<'tx> {
  type BucketType = BucketRwCell<'tx>;

  fn non_physical_rollback(self) -> crate::Result<()> {
    todo!()
  }

  fn close(self) -> crate::Result<()> {
    todo!()
  }
}

impl<'tx> TxRwIAPI<'tx> for TxRwCell<'tx> {
  type CursorRwType = InnerCursor<'tx, Self, Self::BucketType>;

  fn freelist(self) -> RefMut<'tx, Freelist> {
    todo!()
  }

  fn allocate(self, count: usize) -> crate::Result<MutPage<'tx>> {
    todo!()
  }

  fn api_cursor_mut(self) -> Self::CursorRwType {
    let root_bucket = self.root_bucket();
    root_bucket.i_cursor()
  }

  fn api_create_bucket(self, name: &[u8]) -> crate::Result<Self::BucketType> {
    let root_bucket = self.root_bucket();
    root_bucket.api_create_bucket(name)
  }

  fn api_create_bucket_if_not_exist(self, name: &[u8]) -> crate::Result<Self::BucketType> {
    let root_bucket = self.root_bucket();
    root_bucket.api_create_bucket_if_not_exists(name)
  }

  fn api_delete_bucket(self, name: &[u8]) -> crate::Result<()> {
    let root_bucket = self.root_bucket();
    root_bucket.api_delete_bucket(name)
  }

  fn api_commit(self) -> crate::Result<()> {
    todo!("api_commit")
  }
}

pub struct TxImpl<'tx> {
  bump: Pin<Box<Bump>>,
  lock: Pin<Box<RwLockReadGuard<'tx, DBShared>>>,
  tx: Pin<Rc<TxCell<'tx>>>,
}

impl<'tx> TxImpl<'tx> {
  pub(crate) fn new(lock: RwLockReadGuard<'tx, DBShared>) -> TxImpl<'tx> {
    let bump = lock.records.lock().pop_read_bump();
    let meta = lock.backend.meta();
    let page_size = meta.page_size() as usize;
    let inline_bucket = meta.root();
    let mut uninit: MaybeUninit<TxImpl<'tx>> = MaybeUninit::uninit();
    let ptr = uninit.as_mut_ptr();
    unsafe {
      addr_of_mut!((*ptr).bump).write(Box::pin(bump));
      let bump = &(**addr_of!((*ptr).bump));
      addr_of_mut!((*ptr).lock).write(Box::pin(lock));
      let pager: &dyn Pager = &(**addr_of!((*ptr).lock));
      let tx = Rc::new_cyclic(|weak| {
        let r = TxR {
          b: bump,
          page_size,
          pager,
          meta,
          stats: Default::default(),
          is_rollback: false,
          p: Default::default(),
        };
        let bucket = BucketCell::new_in(bump, inline_bucket, weak.clone(), None);
        TxCell {
          cell: BCell::new_in(r, bucket, bump),
        }
      });
      addr_of_mut!((*ptr).tx).write(Pin::new(tx));
      uninit.assume_init()
    }
  }

  pub(crate) fn get_ref(&self) -> TxRef<'tx> {
    TxRef {
      tx: TxCell { cell: self.tx.cell },
    }
  }
}

impl<'tx> Drop for TxImpl<'tx> {
  fn drop(&mut self) {
    let tx_id = self.id();
    let stats = self.stats();
    let mut swap_bump = Bump::with_capacity(0);
    mem::swap(&mut swap_bump, &mut self.bump);
    let mut records = self.lock.records.lock();
    records.remove_tx(tx_id, stats, swap_bump);
  }
}

impl<'tx> TxApi<'tx> for TxImpl<'tx> {
  type CursorType = CursorImpl<'tx, InnerCursor<'tx, TxCell<'tx>, BucketCell<'tx>>>;
  type BucketType = BucketImpl<'tx>;

  fn id(&self) -> TxId {
    self.tx.api_id()
  }

  fn size(&self) -> u64 {
    self.tx.api_size()
  }

  fn writeable(&self) -> bool {
    false
  }

  fn cursor(&self) -> Self::CursorType {
    self.tx.api_cursor().into()
  }

  fn stats(&self) -> TxStats {
    self.tx.api_stats()
  }

  fn bucket(&self, name: &[u8]) -> Option<Self::BucketType> {
    self.tx.api_bucket(name).map(|b| b.into())
  }

  fn for_each<F: FnMut(&[u8], Self::BucketType)>(&self, f: F) -> crate::Result<()> {
    todo!()
  }

  fn rollback(self) -> crate::Result<()> {
    let _ = self.tx.rollback();

    Ok(())
  }

  fn page(&self, id: PgId) -> crate::Result<Option<PageInfo>> {
    self.tx.api_page(id)
  }
}

pub struct TxRef<'tx> {
  tx: TxCell<'tx>,
}

impl<'tx> TxApi<'tx> for TxRef<'tx> {
  type CursorType = CursorImpl<'tx, InnerCursor<'tx, TxCell<'tx>, BucketCell<'tx>>>;
  type BucketType = BucketImpl<'tx>;

  fn id(&self) -> TxId {
    self.tx.api_id()
  }

  fn size(&self) -> u64 {
    self.tx.api_size()
  }

  fn writeable(&self) -> bool {
    false
  }

  fn cursor(&self) -> Self::CursorType {
    self.tx.api_cursor().into()
  }

  fn stats(&self) -> TxStats {
    self.tx.api_stats()
  }

  fn bucket(&self, name: &[u8]) -> Option<Self::BucketType> {
    self.tx.api_bucket(name).map(|b| b.into())
  }

  fn for_each<F: FnMut(&[u8], Self::BucketType)>(&self, f: F) -> crate::Result<()> {
    todo!()
  }

  fn rollback(self) -> crate::Result<()> {
    self.tx.rollback()
  }

  fn page(&self, id: PgId) -> crate::Result<Option<PageInfo>> {
    self.tx.api_page(id)
  }
}

pub struct TxRwImpl<'tx> {
  bump: Pin<Box<Bump>>,
  lock: Pin<Box<RwLockWriteGuard<'tx, DBShared>>>,
  pub(crate) tx: Pin<Rc<TxRwCell<'tx>>>,
}

impl<'tx> TxRwImpl<'tx> {
  pub(crate) fn get_ref(&self) -> TxRwRef<'tx> {
    TxRwRef {
      tx: TxRwCell { cell: self.tx.cell },
    }
  }

  pub(crate) fn new(lock: RwLockWriteGuard<'tx, DBShared>) -> TxRwImpl<'tx> {
    let bump = lock.records.lock().pop_read_bump();
    let meta = lock.backend.meta();
    let page_size = meta.page_size() as usize;
    let inline_bucket = meta.root();
    let mut uninit: MaybeUninit<TxRwImpl<'tx>> = MaybeUninit::uninit();
    let ptr = uninit.as_mut_ptr();
    unsafe {
      addr_of_mut!((*ptr).bump).write(Box::pin(bump));
      let bump = &(**addr_of!((*ptr).bump));
      addr_of_mut!((*ptr).lock).write(Box::pin(lock));
      let pager: &dyn Pager = &(**addr_of!((*ptr).lock));
      let tx = Rc::new_cyclic(|weak| {
        let r = TxR {
          b: bump,
          page_size,
          pager,
          meta,
          stats: Default::default(),
          is_rollback: false,
          p: Default::default(),
        };
        let w = TxW {
          pages: HashMap::new_in(bump),
          commit_handlers: BVec::new_in(bump),
          p: Default::default(),
        };
        let bucket = BucketRwCell::new_in(bump, inline_bucket, weak.clone(), None);
        TxRwCell {
          cell: BCell::new_in(TxRW { r, w }, bucket, bump),
        }
      });
      addr_of_mut!((*ptr).tx).write(Pin::new(tx));
      uninit.assume_init()
    }
  }
}

impl<'tx> Drop for TxRwImpl<'tx> {
  fn drop(&mut self) {
    let tx_id = self.id();
    let stats = self.stats();
    let mut swap_bump = Bump::with_capacity(0);
    mem::swap(&mut swap_bump, &mut self.bump);
    let mut records = self.lock.records.lock();

    // TODO: Reload freelist
    records.remove_tx(tx_id, stats, swap_bump);
  }
}

impl<'tx> TxApi<'tx> for TxRwImpl<'tx> {
  type CursorType = CursorImpl<'tx, InnerCursor<'tx, TxRwCell<'tx>, BucketRwCell<'tx>>>;
  type BucketType = BucketRwImpl<'tx>;

  fn id(&self) -> TxId {
    self.tx.api_id()
  }

  fn size(&self) -> u64 {
    self.tx.api_size()
  }

  fn writeable(&self) -> bool {
    true
  }

  fn cursor(&self) -> Self::CursorType {
    self.tx.api_cursor().into()
  }

  fn stats(&self) -> TxStats {
    self.tx.api_stats()
  }

  fn bucket(&self, name: &[u8]) -> Option<Self::BucketType> {
    self.tx.api_bucket(name).map(|b| b.into())
  }

  fn for_each<F: FnMut(&[u8], Self::BucketType)>(&self, f: F) -> crate::Result<()> {
    //self.tx.api_for_each(f)
    // TODO: mismatching bucket types
    todo!()
  }

  fn rollback(self) -> crate::Result<()> {
    self.tx.rollback()
  }

  fn page(&self, id: PgId) -> crate::Result<Option<PageInfo>> {
    self.tx.api_page(id)
  }
}

impl<'tx> TxRwApi<'tx> for TxRwImpl<'tx> {
  type CursorRwType = CursorRwImpl<'tx, InnerCursor<'tx, TxRwCell<'tx>, BucketRwCell<'tx>>>;

  fn cursor_mut(&mut self) -> Self::CursorRwType {
    self.tx.api_cursor_mut().into()
  }

  fn create_bucket(&mut self, name: &[u8]) -> crate::Result<Self::BucketType> {
    self.tx.api_create_bucket(name).map(|b| b.into())
  }

  fn create_bucket_if_not_exists(&mut self, name: &[u8]) -> crate::Result<Self::BucketType> {
    self
      .tx
      .api_create_bucket_if_not_exist(name)
      .map(|b| b.into())
  }

  fn delete_bucket(&mut self, name: &[u8]) -> crate::Result<()> {
    self.tx.api_delete_bucket(name)
  }

  fn commit(self) -> crate::Result<()> {
    self.tx.api_commit()
  }
}

pub struct TxRwRef<'tx> {
  tx: TxRwCell<'tx>,
}

impl<'tx> TxApi<'tx> for TxRwRef<'tx> {
  type CursorType = CursorImpl<'tx, InnerCursor<'tx, TxRwCell<'tx>, BucketRwCell<'tx>>>;
  type BucketType = BucketRwImpl<'tx>;

  fn id(&self) -> TxId {
    self.tx.api_id()
  }

  fn size(&self) -> u64 {
    self.tx.api_size()
  }

  fn writeable(&self) -> bool {
    true
  }

  fn cursor(&self) -> Self::CursorType {
    self.tx.api_cursor().into()
  }

  fn stats(&self) -> TxStats {
    self.tx.api_stats()
  }

  fn bucket(&self, name: &[u8]) -> Option<Self::BucketType> {
    self.tx.api_bucket(name).map(|b| b.into())
  }

  fn for_each<F: FnMut(&[u8], Self::BucketType)>(&self, f: F) -> crate::Result<()> {
    todo!()
  }

  fn rollback(self) -> crate::Result<()> {
    self.tx.rollback()
  }

  fn page(&self, id: PgId) -> crate::Result<Option<PageInfo>> {
    self.tx.api_page(id)
  }
}

impl<'tx> TxRwApi<'tx> for TxRwRef<'tx> {
  type CursorRwType = CursorRwImpl<'tx, InnerCursor<'tx, TxRwCell<'tx>, BucketRwCell<'tx>>>;

  fn cursor_mut(&mut self) -> Self::CursorRwType {
    self.tx.api_cursor_mut().into()
  }

  fn create_bucket(&mut self, name: &[u8]) -> crate::Result<Self::BucketType> {
    self.tx.api_create_bucket(name).map(|b| b.into())
  }

  fn create_bucket_if_not_exists(&mut self, name: &[u8]) -> crate::Result<Self::BucketType> {
    self
      .tx
      .api_create_bucket_if_not_exist(name)
      .map(|b| b.into())
  }

  fn delete_bucket(&mut self, name: &[u8]) -> crate::Result<()> {
    self.tx.api_delete_bucket(name)
  }

  fn commit(self) -> crate::Result<()> {
    self.tx.api_commit()
  }
}

pub(crate) struct TypeA<'a> {
  pub b: &'a RefCell<TypeB<'a>>,
  pub i: usize,
}

pub(crate) struct TypeB<'a> {
  pub a: &'a RefCell<TypeA<'a>>,
  pub i: usize,
}

pub(crate) fn create_cycle<'a>(bump: &'a Bump) -> (&RefCell<TypeA<'a>>, &RefCell<TypeB<'a>>) {
  let mut uninit_a: MaybeUninit<TypeA<'a>> = MaybeUninit::uninit();
  println!("uninit: {}", mem::size_of::<MaybeUninit<TypeA<'a>>>());
  let mut uninit_b: MaybeUninit<TypeB<'a>> = MaybeUninit::uninit();
  let cell_a = bump.alloc(RefCell::new(uninit_a));
  let ptr_a = cell_a.borrow_mut().as_mut_ptr();
  let cell_b = bump.alloc(RefCell::new(uninit_b));
  let ptr_b = cell_b.borrow_mut().as_mut_ptr();
  unsafe {
    let cell_a_t = mem::transmute::<&mut RefCell<MaybeUninit<TypeA>>, &RefCell<TypeA>>(cell_a);
    println!("uninit: {}", mem::size_of::<TypeA>());
    let cell_b_t = mem::transmute::<&mut RefCell<MaybeUninit<TypeB>>, &RefCell<TypeB>>(cell_b);
    addr_of_mut!((*ptr_a).b).write(cell_b_t);
    addr_of_mut!((*ptr_a).i).write(1);
    addr_of_mut!((*ptr_b).a).write(cell_a_t);
    addr_of_mut!((*ptr_b).i).write(100);
    cell_a.borrow_mut().assume_init_mut();
    cell_b.borrow_mut().assume_init_mut();
    (cell_a_t, cell_b_t)
  }
}

#[cfg(test)]
mod test {
  use crate::tx::create_cycle;
  use bumpalo::Bump;

  #[test]
  fn create_cycle_test() -> crate::Result<()> {
    let bump = Bump::new();
    let (a, b) = create_cycle(&bump);
    assert_eq!(100, a.borrow().b.borrow().i);
    assert_eq!(1, b.borrow().a.borrow().i);
    Ok(())
  }
}
