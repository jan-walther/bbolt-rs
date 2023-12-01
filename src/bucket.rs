use crate::common::bucket::{InBucket, IN_BUCKET_SIZE};
use crate::common::memory::{IsAligned, SCell};
use crate::common::meta::MetaPage;
use crate::common::page::{CoerciblePage, Page, RefPage, BUCKET_LEAF_FLAG, PAGE_HEADER_SIZE};
use crate::common::tree::{MappedBranchPage, TreePage, LEAF_PAGE_ELEMENT_SIZE};
use crate::common::{BVec, HashMap, IRef, PgId, ZERO_PGID};
use crate::cursor::{Cursor, CursorAPI, CursorIAPI, CursorMut, CursorMutIAPI, ElemRef, ICursor};
use crate::node::{NodeImpl, NodeMut, NodeW};
use crate::tx::{Tx, TxAPI, TxIAPI, TxIRef, TxImpl, TxMut, TxMutIAPI, TxR, TxW};
use crate::Error;
use crate::Error::{
  BucketExists, BucketNameRequired, BucketNotFound, IncompatibleValue, KeyRequired, KeyTooLarge,
  ValueTooLarge,
};
use bumpalo::Bump;
use bytemuck::{Pod, Zeroable};
use either::Either;
use std::alloc::Layout;
use std::cell::{Ref, RefCell, RefMut};
use std::marker::PhantomData;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::ptr::slice_from_raw_parts_mut;

const DEFAULT_FILL_PERCENT: f64 = 0.5;
const MAX_KEY_SIZE: u32 = 32768;
const MAX_VALUE_SIZE: u32 = (1 << 31) - 2;
const INLINE_PAGE_ALIGNMENT: usize = mem::align_of::<InlinePage>();
const INLINE_PAGE_SIZE: usize = mem::size_of::<InlinePage>();

#[repr(C)]
#[derive(Copy, Clone, Default, Pod, Zeroable)]
struct InlinePage {
  header: InBucket,
  page: Page,
}

pub struct BucketStats {}

pub(crate) trait BucketIAPI<'tx>: Copy + Clone + 'tx {
  type TxType: TxIRef<'tx>;
  type BucketType: BucketIRef<'tx>;

  fn new(
    bucket_header: InBucket, tx: &'tx Self::TxType, inline_page: Option<RefPage<'tx>>,
  ) -> Self::BucketType;

  fn is_writeable(&self) -> bool;

  fn tx(&self) -> &'tx Self::TxType;

  fn max_inline_bucket_size(&self) -> usize {
    self.tx().page_size() / 4
  }
}

pub trait BucketIRef<'tx>:
  BucketIAPI<'tx> + IRef<BucketR<'tx>, BucketP<'tx, Self::BucketType>>
{
}

pub(crate) struct BucketImpl {}

impl BucketImpl {
  pub(crate) fn api_tx<'tx, B: BucketIRef<'tx>>(cell: B) -> &'tx B::TxType {
    cell.tx()
  }

  pub(crate) fn root<'tx, B: BucketIRef<'tx>>(cell: B) -> PgId {
    cell.borrow_iref().0.bucket_header.root()
  }

  pub(crate) fn i_cursor<'tx, B: BucketIRef<'tx>>(cell: B) -> ICursor<'tx, B> {
    ICursor::new(cell, cell.tx().bump())
  }

  pub(crate) fn api_bucket<'tx, B: BucketIRef<'tx>>(cell: B, name: &[u8]) -> Option<B::BucketType> {
    if cell.is_writeable() {
      let b = cell.borrow_iref();
      if let Some(w) = b.1 {
        if let Some(child) = w.buckets.get(name) {
          return Some(*child);
        }
      }
    }
    let mut c = BucketImpl::i_cursor(cell);
    let (k, v, flags) = c.i_seek(name)?;
    if !(name == k) || (flags & BUCKET_LEAF_FLAG) == 0 {
      return None;
    }

    let child = BucketImpl::open_bucket(cell, v);
    if let Some(mut w) = cell.borrow_mut_iref().1 {
      let bump = cell.tx().bump();
      let name = bump.alloc_slice_copy(name);
      w.buckets.insert(name, child);
    }

    Some(child)
  }

  fn open_bucket<'tx, B: BucketIRef<'tx>>(
    cell: B, mut value: &'tx [u8],
  ) -> <B as BucketIAPI<'tx>>::BucketType {
    // 3 goals

    // Unaligned access requires a copy to be made.
    //TODO: use std is_aligned_to when it comes out
    if !IsAligned::is_aligned_to::<InlinePage>(value.as_ptr()) {
      // TODO: Shove this into a centralized function somewhere
      let layout = Layout::from_size_align(value.len(), INLINE_PAGE_ALIGNMENT).unwrap();
      let bump = cell.tx().bump();
      let new_value = unsafe {
        let mut new_value = bump.alloc_layout(layout);
        let new_value_ptr = new_value.as_mut() as *mut u8;
        &mut *slice_from_raw_parts_mut(new_value_ptr, value.len())
      };
      new_value.copy_from_slice(value);
      value = new_value;
    }
    let bucket_header = *bytemuck::from_bytes::<InBucket>(value);
    let ref_page = if bucket_header.root() == ZERO_PGID {
      assert!(
        value.len() >= INLINE_PAGE_SIZE,
        "subbucket value not large enough. Expected at least {} bytes. Was {}",
        INLINE_PAGE_SIZE,
        value.len()
      );
      unsafe {
        let ref_page_ptr = value.as_ptr().add(IN_BUCKET_SIZE);
        Some(RefPage::new(ref_page_ptr))
      }
    } else {
      None
    };
    B::new(bucket_header, cell.tx(), ref_page)
  }

  pub(crate) fn api_create_bucket<'tx>(
    cell: BucketMut<'tx>, key: &[u8],
  ) -> crate::Result<BucketMut<'tx>> {
    if key.is_empty() {
      return Err(BucketNameRequired);
    }
    let mut c = BucketImpl::i_cursor(cell);

    if let Some((k, _, flags)) = c.i_seek(key) {
      if k == key {
        if flags & BUCKET_LEAF_FLAG != 0 {
          return Err(BucketExists);
        }
        return Err(IncompatibleValue);
      }
    }

    let mut inline_page = InlinePage::default();
    inline_page.page.set_leaf();
    let layout = Layout::from_size_align(INLINE_PAGE_SIZE, INLINE_PAGE_ALIGNMENT).unwrap();
    let bump = cell.tx.bump();
    let value = unsafe {
      let mut data = bump.alloc_layout(layout);
      &mut *slice_from_raw_parts_mut(data.as_mut() as *mut u8, INLINE_PAGE_SIZE)
    };
    value.copy_from_slice(bytemuck::bytes_of(&inline_page));
    let key = bump.alloc_slice_clone(key) as &[u8];

    NodeImpl::put(c.node(), key, key, value, ZERO_PGID, BUCKET_LEAF_FLAG);

    cell.borrow_mut_iref().0.inline_page = None;

    return Ok(BucketImpl::api_bucket(cell, key).unwrap());
  }

  pub(crate) fn api_create_bucket_if_not_exists<'tx>(
    cell: BucketMut<'tx>, key: &[u8],
  ) -> crate::Result<BucketMut<'tx>> {
    match BucketImpl::api_create_bucket(cell, key) {
      Ok(child) => Ok(child),
      Err(error) => {
        if error == BucketExists {
          return Ok(BucketImpl::api_bucket(cell, key).unwrap());
        } else {
          return Err(error);
        }
      }
    }
  }

  pub(crate) fn api_delete_bucket<'tx>(cell: BucketMut<'tx>, key: &[u8]) -> crate::Result<()> {
    let mut c = BucketImpl::i_cursor(cell);

    let (k, _, flags) = c.i_seek(key).unwrap();
    if key != k {
      return Err(BucketNotFound);
    } else if flags & BUCKET_LEAF_FLAG != 0 {
      return Err(IncompatibleValue);
    }

    let child = BucketImpl::api_bucket(cell, key).unwrap();
    BucketImpl::api_for_each_bucket(child, |k| {
      match BucketImpl::api_delete_bucket(cell, k) {
        Ok(_) => Ok(()),
        // TODO: Ideally we want to properly chain errors here
        Err(e) => Err(Error::Other(e.into())),
      }
    })?;

    if let Some(mut w) = cell.borrow_mut_iref().1 {
      w.buckets.remove(key);
    }

    if let Some(mut w) = child.borrow_mut_iref().1 {
      w.nodes.clear();
      w.root_node = None;
    }
    BucketImpl::free(child);

    NodeImpl::del(c.node(), key);
    Ok(())
  }

  fn api_get<'tx, B: BucketIRef<'tx>>(cell: B, key: &[u8]) -> Option<&'tx [u8]> {
    let (k, v, flags) = BucketImpl::i_cursor(cell).i_seek(key).unwrap();
    if (flags & BUCKET_LEAF_FLAG) != 0 {
      return None;
    }
    if key != k {
      return None;
    }
    Some(v)
  }

  fn api_put<'tx>(cell: BucketMut<'tx>, key: &[u8], value: &[u8]) -> crate::Result<()> {
    if key.is_empty() {
      return Err(KeyRequired);
    } else if key.len() > MAX_KEY_SIZE as usize {
      return Err(KeyTooLarge);
    } else if value.len() > MAX_VALUE_SIZE as usize {
      return Err(ValueTooLarge);
    }
    let mut c = BucketImpl::i_cursor(cell);
    let (k, _, flags) = c.i_seek(key).unwrap();

    if (flags & BUCKET_LEAF_FLAG) != 0 || key != k {
      return Err(IncompatibleValue);
    }
    let bump = cell.tx().bump();
    let key = &*bump.alloc_slice_clone(key);
    NodeImpl::put(c.node(), key, key, value, ZERO_PGID, 0);
    Ok(())
  }

  fn api_delete<'tx>(cell: BucketMut<'tx>, key: &[u8]) -> crate::Result<()> {
    let mut c = BucketImpl::i_cursor(cell);
    let (k, _, flags) = c.i_seek(key).unwrap();

    if key != k {
      return Ok(());
    }

    if flags & BUCKET_LEAF_FLAG != 0 {
      return Err(IncompatibleValue);
    }

    NodeImpl::del(c.node(), key);

    Ok(())
  }

  fn api_sequence<'tx, B: BucketIRef<'tx>>(cell: B) -> u64 {
    cell.borrow_iref().0.bucket_header.sequence()
  }

  fn api_set_sequence<'tx>(cell: BucketMut<'tx>, v: u64) -> crate::Result<()> {
    // TODO: Since this is repeated a bunch, let materialize root in a single function
    let mut materialize_root = None;
    if let (r, Some(w)) = cell.borrow_iref() {
      materialize_root = match w.root_node {
        None => Some(r.bucket_header.root()),
        Some(_) => None,
      }
    }

    materialize_root.and_then(|root| Some(BucketImpl::node(cell, root, None)));

    cell.borrow_mut_iref().0.bucket_header.set_sequence(v);
    Ok(())
  }

  fn api_next_sequence<'tx>(cell: BucketMut<'tx>) -> crate::Result<u64> {
    // TODO: Since this is repeated a bunch, let materialize root in a single function
    let mut materialize_root = None;
    if let (r, Some(w)) = cell.borrow_iref() {
      materialize_root = match w.root_node {
        None => Some(r.bucket_header.root()),
        Some(_) => None,
      }
    }
    materialize_root.and_then(|root| Some(BucketImpl::node(cell, root, None)));

    let mut r = cell.borrow_mut_iref().0;
    r.bucket_header.inc_sequence();
    Ok(r.bucket_header.sequence())
  }

  fn api_for_each<'tx, B: BucketIRef<'tx>, F: Fn(&[u8]) -> crate::Result<()>>(
    cell: B, f: F,
  ) -> crate::Result<()> {
    let mut c = BucketImpl::i_cursor(cell);
    let mut inode = c.i_first();
    while let Some((k, _, flags)) = inode {
      f(k)?;
      inode = c.i_next();
    }
    Ok(())
  }

  fn api_for_each_bucket<'tx, B: BucketIRef<'tx>, F: FnMut(&[u8]) -> crate::Result<()>>(
    cell: B, mut f: F,
  ) -> crate::Result<()> {
    let mut c = BucketImpl::i_cursor(cell);
    let mut inode = c.i_first();
    while let Some((k, _, flags)) = inode {
      if flags & BUCKET_LEAF_FLAG != 0 {
        f(k)?;
      }
      inode = c.i_next();
    }
    Ok(())
  }

  pub(crate) fn for_each_page<
    'tx,
    B: BucketIRef<'tx>,
    F: FnMut(&RefPage, usize, &[PgId]) + Copy,
  >(
    cell: B, mut f: F,
  ) {
    let root = {
      let (r, _) = cell.borrow_iref();
      let root = r.bucket_header.root();
      if let Some(page) = &r.inline_page {
        f(page, 0, &[root]);
        return;
      }
      root
    };

    TxImpl::for_each_page(cell.tx(), root, f);
  }

  pub fn for_each_page_node<
    'tx,
    B: BucketIRef<'tx>,
    F: FnMut(&Either<RefPage, NodeMut<'tx>>, usize) + Copy,
  >(
    cell: B, mut f: F,
  ) {
    let root = {
      let (r, _) = cell.borrow_iref();
      if let Some(page) = &r.inline_page {
        f(&Either::Left(*page), 0);
        return;
      }
      r.bucket_header.root()
    };
    BucketImpl::_for_each_page_node(cell, root, 0, f);
  }

  fn _for_each_page_node<
    'tx,
    B: BucketIRef<'tx>,
    F: FnMut(&Either<RefPage, NodeMut<'tx>>, usize) + Copy,
  >(
    cell: B, root: PgId, depth: usize, mut f: F,
  ) {
    let pn = BucketImpl::page_node(cell, root);
    f(&pn, depth);
    match &pn {
      Either::Left(page) => {
        if let Some(branch_page) = MappedBranchPage::coerce_ref(page) {
          branch_page.elements().iter().for_each(|elem| {
            BucketImpl::_for_each_page_node(cell, elem.pgid(), depth + 1, f);
          });
        }
      }
      Either::Right(node) => {
        let bump = cell.tx().bump();
        // To keep with our rules we much copy the inode pgids to temporary storage first
        // This should be unnecessary, but working first *then* optimize
        let v = {
          let node_borrow = node.cell.borrow();
          let mut v = BVec::with_capacity_in(node_borrow.inodes.len(), bump);
          let ids = node_borrow.inodes.iter().map(|inode| inode.pgid());
          v.extend(ids);
          v
        };
        v.into_iter()
          .for_each(|pgid| BucketImpl::_for_each_page_node(cell, pgid, depth + 1, f));
      }
    }
  }

  pub fn page_node<'tx, B: BucketIRef<'tx>>(
    cell: B, id: PgId,
  ) -> Either<RefPage<'tx>, NodeMut<'tx>> {
    let (r, w) = cell.borrow_iref();
    // Inline buckets have a fake page embedded in their value so treat them
    // differently. We'll return the rootNode (if available) or the fake page.
    if r.bucket_header.root() == ZERO_PGID {
      if id != ZERO_PGID {
        panic!("inline bucket non-zero page access(2): {} != 0", id)
      }
      if let Some(root_node) = &w.map(|wb| wb.root_node).flatten() {
        return Either::Right(*root_node);
      } else {
        return Either::Left(r.inline_page.unwrap());
      }
    }

    if cell.is_writeable() {
      // Check the node cache for non-inline buckets.
      if let Some(wb) = &w {
        if let Some(node) = wb.nodes.get(&id) {
          return Either::Right(*node);
        }
      }
    }
    Either::Left(TxImpl::page(cell.tx(), id))
  }

  pub(crate) fn spill<'tx>(cell: BucketMut<'tx>, bump: &'tx Bump) -> crate::Result<()> {
    // To keep with our rules we much copy the bucket entries to temporary storage first
    // This should be unnecessary, but working first *then* optimize
    let v = {
      let bucket_mut = cell.borrow_iref();
      let w = bucket_mut.1.unwrap();
      let mut v = BVec::with_capacity_in(w.buckets.len(), bump);
      // v.extend() would be more idiomatic, but I'm too tired atm to figure out why
      // it's not working
      w.buckets.iter().for_each(|(k, b)| {
        v.push((*k, *b));
      });
      v
    };

    for (name, child) in v.into_iter() {}

    Ok(())
  }

  /// inlineable returns true if a bucket is small enough to be written inline
  /// and if it contains no subbuckets. Otherwise returns false.
  pub(crate) fn inlineable<'tx>(cell: BucketMut<'tx>) -> bool {
    let b = cell.borrow_iref();
    let w = b.1.unwrap();

    // Bucket must only contain a single leaf node.
    let n = match w.root_node {
      None => return false,
      Some(n) => n,
    };
    let node_ref = n.cell.borrow();
    if node_ref.is_leaf {
      return false;
    }

    // Bucket is not inlineable if it contains subbuckets or if it goes beyond
    // our threshold for inline bucket size.
    let mut size = PAGE_HEADER_SIZE;
    for inode in &node_ref.inodes {
      size += LEAF_PAGE_ELEMENT_SIZE + inode.key().len() + inode.value().len();

      if inode.flags() & BUCKET_LEAF_FLAG != 0 {
        return false;
      } else if size > cell.max_inline_bucket_size() {
        return false;
      }
    }

    true
  }

  pub(crate) fn free<'tx>(cell: BucketMut<'tx>) {
    if cell.borrow_iref().0.bucket_header.root() == ZERO_PGID {
      return;
    }

    let txid = cell.tx.meta().txid();

    BucketImpl::for_each_page_node(cell, |pn, depth| match pn {
      Either::Left(page) => cell.tx().freelist().free(txid, page),
      Either::Right(node) => NodeImpl::free(*node),
    });
  }

  pub(crate) fn own_in<'tx>(cell: BucketMut<'tx>) {
    let bump = cell.tx().bump();
    let (root, children) = {
      let (r, w) = cell.borrow_iref();
      let wb = w.unwrap();
      let mut children: BVec<BucketMut<'tx>> = BVec::with_capacity_in(wb.buckets.len(), bump);
      children.extend(wb.buckets.values());
      (wb.root_node, children)
    };

    if let Some(node) = root {
      NodeImpl::own_in(NodeImpl::root(node), bump)
    }

    for child in children.into_iter() {
      BucketImpl::own_in(child)
    }
  }

  pub(crate) fn node<'tx>(
    cell: BucketMut<'tx>, pgid: PgId, parent: Option<NodeMut<'tx>>,
  ) -> NodeMut<'tx> {
    let inline_page = {
      let (r, w) = cell.borrow_mut_iref();
      let wb = w.unwrap();

      if let Some(n) = wb.nodes.get(&pgid) {
        return *n;
      }
      r.inline_page
    };

    let page = match inline_page {
      None => cell.tx().page(pgid),
      Some(page) => page,
    };

    let n = NodeMut::read_in(cell, parent, &page);
    let (r, w) = cell.borrow_mut_iref();
    let mut wb = w.unwrap();
    wb.nodes.insert(pgid, n);
    n
  }
}

pub trait BucketAPI<'tx>: Copy + Clone + 'tx  {
  fn root(&self) -> PgId;

  fn writeable(&self) -> bool;

  fn cursor(&self) -> Cursor<'tx>;

  fn bucket(&self, name: &[u8]) -> Self;

  fn get(&self, key: &[u8]) -> &'tx [u8];

  fn sequence(&self) -> u64;

  fn for_each<F: Fn(&[u8]) -> crate::Result<()>>(&self, f: F) -> crate::Result<()>;

  fn for_each_bucket<F: Fn(&[u8]) -> crate::Result<()>>(&self, f: F) -> crate::Result<()>;

  fn status(&self) -> BucketStats;
}

pub trait BucketMutAPI<'tx>: BucketAPI<'tx> {

  type BucketType: BucketMutAPI<'tx>;

  fn create_bucket(&mut self, key: &[u8]) -> crate::Result<Self::BucketType>;

  fn create_bucket_if_not_exists(&mut self, key: &[u8]) -> crate::Result<Self::BucketType>;

  fn cursor_mut(&self) -> CursorMut<'tx>;

  fn delete_bucket(&mut self, key: &[u8]) -> crate::Result<()>;

  fn put(&mut self, key: &[u8], data: &[u8]) -> crate::Result<()>;

  fn delete(&mut self, key: &[u8]) -> crate::Result<()>;

  fn set_sequence(&mut self, v: u64) -> crate::Result<()>;

  fn next_sequence(&mut self) -> crate::Result<u64>;

  fn for_each_mut<F: Fn(&[u8]) -> crate::Result<()>>(&mut self, f: F) -> crate::Result<()>;

  fn for_each_bucket_mut<F: Fn(&[u8]) -> crate::Result<()>>(&mut self, f: F) -> crate::Result<()>;
}

pub struct BucketR<'tx> {
  pub(crate) bucket_header: InBucket,
  pub(crate) inline_page: Option<RefPage<'tx>>,
  p: PhantomData<&'tx u8>,
}

impl<'tx> BucketR<'tx> {
  pub fn new(in_bucket: InBucket) -> BucketR<'tx> {
    BucketR {
      bucket_header: in_bucket,
      inline_page: None,
      p: Default::default(),
    }
  }
}

pub struct BucketP<'tx, B: BucketIRef<'tx>> {
  root_node: Option<NodeMut<'tx>>,
  buckets: HashMap<'tx, &'tx [u8], B>,
  nodes: HashMap<'tx, PgId, NodeMut<'tx>>,
  fill_percent: f64,
}

impl<'tx, B: BucketIRef<'tx>> BucketP<'tx, B> {
  pub fn new_in(bump: &'tx Bump) -> BucketP<'tx, B> {
    BucketP {
      root_node: None,
      buckets: HashMap::new_in(bump),
      nodes: HashMap::new_in(bump),
      fill_percent: DEFAULT_FILL_PERCENT,
    }
  }
}

pub type BucketW<'tx> = BucketP<'tx, BucketMut<'tx>>;

pub struct BucketRW<'tx> {
  r: BucketR<'tx>,
  w: BucketW<'tx>,
}

impl<'tx> BucketRW<'tx> {
  pub fn new_in(bump: &'tx Bump, in_bucket: InBucket) -> BucketRW<'tx> {
    BucketRW {
      r: BucketR::new(in_bucket),
      w: BucketW::new_in(bump),
    }
  }
}

#[derive(Copy, Clone)]
pub struct Bucket<'tx> {
  tx: &'tx Tx<'tx>,
  cell: SCell<'tx, BucketR<'tx>>,
}

impl<'tx> BucketIAPI<'tx> for Bucket<'tx> {
  type TxType = Tx<'tx>;
  type BucketType = Bucket<'tx>;

  fn new(
    bucket_header: InBucket, tx: &'tx Self::TxType, inline_page: Option<RefPage<'tx>>,
  ) -> Self::BucketType {
    let r = BucketR {
      bucket_header,
      inline_page,
      p: Default::default(),
    };

    Bucket {
      tx,
      cell: SCell::new_in(r, tx.bump()),
    }
  }

  #[inline(always)]
  fn is_writeable(&self) -> bool {
    false
  }

  fn tx(&self) -> &'tx Self::TxType {
    &self.tx
  }
}

impl<'tx> IRef<BucketR<'tx>, BucketP<'tx, Bucket<'tx>>> for Bucket<'tx> {
  fn borrow_iref(&self) -> (Ref<BucketR<'tx>>, Option<Ref<BucketP<'tx, Bucket<'tx>>>>) {
    (self.cell.borrow(), None)
  }

  fn borrow_mut_iref(
    &self,
  ) -> (
    RefMut<BucketR<'tx>>,
    Option<RefMut<BucketP<'tx, Bucket<'tx>>>>,
  ) {
    (self.cell.borrow_mut(), None)
  }
}

impl<'tx> BucketIRef<'tx> for Bucket<'tx> {}

impl<'tx> BucketAPI<'tx> for Bucket<'tx> {
  fn root(&self) -> PgId {
    todo!()
  }

  fn writeable(&self) -> bool {
    todo!()
  }

  fn cursor(&self) -> Cursor<'tx> {
    todo!()
  }

  fn bucket(&self, name: &[u8]) -> Self {
    todo!()
  }

  fn get(&self, key: &[u8]) -> &'tx [u8] {
    todo!()
  }

  fn sequence(&self) -> u64 {
    todo!()
  }

  fn for_each<F: Fn(&[u8]) -> crate::Result<()>>(&self, f: F) -> crate::Result<()> {
    todo!()
  }

  fn for_each_bucket<F: Fn(&[u8]) -> crate::Result<()>>(&self, f: F) -> crate::Result<()> {
    todo!()
  }

  fn status(&self) -> BucketStats {
    todo!()
  }
}

#[derive(Copy, Clone)]
pub struct BucketMut<'tx> {
  tx: &'tx TxMut<'tx>,
  cell: SCell<'tx, BucketRW<'tx>>,
}

impl<'tx> IRef<BucketR<'tx>, BucketW<'tx>> for BucketMut<'tx> {
  fn borrow_iref(&self) -> (Ref<BucketR<'tx>>, Option<Ref<BucketW<'tx>>>) {
    let (r, w) = Ref::map_split(self.cell.borrow(), |b| (&b.r, &b.w));
    (r, Some(w))
  }

  fn borrow_mut_iref(&self) -> (RefMut<BucketR<'tx>>, Option<RefMut<BucketW<'tx>>>) {
    let (r, w) = RefMut::map_split(self.cell.borrow_mut(), |b| (&mut b.r, &mut b.w));
    (r, Some(w))
  }
}

impl<'tx> BucketIAPI<'tx> for BucketMut<'tx> {
  type TxType = TxMut<'tx>;
  type BucketType = BucketMut<'tx>;

  fn new(
    bucket_header: InBucket, tx: &'tx Self::TxType, inline_page: Option<RefPage<'tx>>,
  ) -> Self::BucketType {
    let r = BucketR {
      bucket_header,
      inline_page,
      p: Default::default(),
    };

    let bump = tx.bump();
    let w = BucketW::new_in(bump);

    BucketMut {
      tx,
      cell: SCell::new_in(BucketRW { r, w }, bump),
    }
  }

  #[inline(always)]
  fn is_writeable(&self) -> bool {
    true
  }

  fn tx(&self) -> &'tx Self::TxType {
    &self.tx
  }
}

impl<'tx> BucketIRef<'tx> for BucketMut<'tx> {}

impl<'tx> BucketAPI<'tx> for BucketMut<'tx> {
  fn root(&self) -> PgId {
    todo!()
  }

  fn writeable(&self) -> bool {
    todo!()
  }

  fn cursor(&self) -> Cursor<'tx> {
    todo!()
  }

  fn bucket(&self, name: &[u8]) -> Self {
    todo!()
  }

  fn get(&self, key: &[u8]) -> &'tx [u8] {
    todo!()
  }

  fn sequence(&self) -> u64 {
    todo!()
  }

  fn for_each<F: Fn(&[u8]) -> crate::Result<()>>(&self, f: F) -> crate::Result<()> {
    todo!()
  }

  fn for_each_bucket<F: Fn(&[u8]) -> crate::Result<()>>(&self, f: F) -> crate::Result<()> {
    todo!()
  }

  fn status(&self) -> BucketStats {
    todo!()
  }
}
impl<'tx> BucketMutAPI<'tx> for BucketMut<'tx> {
  type BucketType = Self;

  fn create_bucket(&mut self, key: &[u8]) -> crate::Result<Self> {
    todo!()
  }

  fn create_bucket_if_not_exists(&mut self, key: &[u8]) -> crate::Result<Self> {
    todo!()
  }

  fn cursor_mut(&self) -> CursorMut<'tx> {
    todo!()
  }

  fn delete_bucket(&mut self, key: &[u8]) -> crate::Result<()> {
    todo!()
  }

  fn put(&mut self, key: &[u8], data: &[u8]) -> crate::Result<()> {
    todo!()
  }

  fn delete(&mut self, key: &[u8]) -> crate::Result<()> {
    todo!()
  }

  fn set_sequence(&mut self, v: u64) -> crate::Result<()> {
    todo!()
  }

  fn next_sequence(&mut self) -> crate::Result<u64> {
    todo!()
  }

  fn for_each_mut<F: Fn(&[u8]) -> crate::Result<()>>(&mut self, f: F) -> crate::Result<()> {
    todo!()
  }

  fn for_each_bucket_mut<F: Fn(&[u8]) -> crate::Result<()>>(&mut self, f: F) -> crate::Result<()> {
    todo!()
  }
}
