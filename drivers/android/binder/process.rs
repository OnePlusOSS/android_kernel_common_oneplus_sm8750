// SPDX-License-Identifier: GPL-2.0

// Copyright (C) 2024 Google LLC.

//! This module defines the `Process` type, which represents a process using a particular binder
//! context.
//!
//! The `Process` object keeps track of all of the resources that this process owns in the binder
//! context.
//!
//! There is one `Process` object for each binder fd that a process has opened, so processes using
//! several binder contexts have several `Process` objects. This ensures that the contexts are
//! fully separated.

use kernel::{
    bindings,
    cred::Credential,
    file::{self, File},
    list::{HasListLinks, List, ListArc, ListArcField, ListArcSafe, ListItem, ListLinks},
    mm,
    page_range::ShrinkablePageRange,
    prelude::*,
    rbtree::{self, RBTree},
    seq_file::SeqFile,
    seq_print,
    sync::poll::PollTable,
    sync::{
        lock::Guard, Arc, ArcBorrow, CondVar, CondVarTimeoutResult, Mutex, SpinLock, UniqueArc,
    },
    task::Task,
    types::{ARef, Either},
    uaccess::{UserSlice, UserSliceReader},
    workqueue::{self, Work},
};

use crate::{
    allocation::{Allocation, AllocationInfo},
    context::Context,
    defs::*,
    error::{BinderError, BinderResult},
    node::{CouldNotDeliverCriticalIncrement, CritIncrWrapper, Node, NodeDeath, NodeRef},
    prio::{self, BinderPriority},
    range_alloc::{self, RangeAllocator},
    thread::{PushWorkRes, Thread},
    DArc, DLArc, DTRWrap, DeliverToRead,
};

use core::mem::take;

struct Mapping {
    address: usize,
    alloc: RangeAllocator<AllocationInfo>,
}

impl Mapping {
    fn new(address: usize, size: usize) -> Result<Self> {
        let alloc = RangeAllocator::new(size)?;
        Ok(Self { address, alloc })
    }
}

// bitflags for defer_work.
const PROC_DEFER_FLUSH: u8 = 1;
const PROC_DEFER_RELEASE: u8 = 2;

/// The fields of `Process` protected by the spinlock.
pub(crate) struct ProcessInner {
    is_manager: bool,
    pub(crate) is_dead: bool,
    threads: RBTree<i32, Arc<Thread>>,
    /// INVARIANT: Threads pushed to this list must be owned by this process.
    ready_threads: List<Thread>,
    nodes: RBTree<u64, DArc<Node>>,
    mapping: Option<Mapping>,
    work: List<DTRWrap<dyn DeliverToRead>>,
    delivered_deaths: List<DTRWrap<NodeDeath>, 2>,

    /// The number of requested threads that haven't registered yet.
    requested_thread_count: u32,
    /// The maximum number of threads used by the process thread pool.
    max_threads: u32,
    /// The number of threads the started and registered with the thread pool.
    started_thread_count: u32,

    /// Bitmap of deferred work to do.
    defer_work: u8,

    /// Number of transactions to be transmitted before processes in freeze_wait
    /// are woken up.
    outstanding_txns: u32,
    /// Process is frozen and unable to service binder transactions.
    pub(crate) is_frozen: bool,
    /// Process received sync transactions since last frozen.
    pub(crate) sync_recv: bool,
    /// Process received async transactions since last frozen.
    pub(crate) async_recv: bool,
    /// Check for oneway spam
    oneway_spam_detection_enabled: bool,
}

impl ProcessInner {
    fn new() -> Self {
        Self {
            is_manager: false,
            is_dead: false,
            threads: RBTree::new(),
            ready_threads: List::new(),
            mapping: None,
            nodes: RBTree::new(),
            work: List::new(),
            delivered_deaths: List::new(),
            requested_thread_count: 0,
            max_threads: 0,
            started_thread_count: 0,
            defer_work: 0,
            outstanding_txns: 0,
            is_frozen: false,
            sync_recv: false,
            async_recv: false,
            oneway_spam_detection_enabled: false,
        }
    }

    /// Schedule the work item for execution on this process.
    ///
    /// If any threads are ready for work, then the work item is given directly to that thread and
    /// it is woken up. Otherwise, it is pushed to the process work list.
    ///
    /// This call can fail only if the process is dead. In this case, the work item is returned to
    /// the caller so that the caller can drop it after releasing the inner process lock. This is
    /// necessary since the destructor of `Transaction` will take locks that can't necessarily be
    /// taken while holding the inner process lock.
    pub(crate) fn push_work(
        &mut self,
        work: DLArc<dyn DeliverToRead>,
    ) -> Result<(), (BinderError, DLArc<dyn DeliverToRead>)> {
        // Try to find a ready thread to which to push the work.
        if let Some(thread) = self.ready_threads.pop_front() {
            work.on_thread_selected(&thread);

            // Push to thread while holding state lock. This prevents the thread from giving up
            // (for example, because of a signal) when we're about to deliver work.
            match thread.push_work(work) {
                PushWorkRes::Ok => Ok(()),
                PushWorkRes::FailedDead(work) => Err((BinderError::new_dead(), work)),
            }
        } else if self.is_dead {
            Err((BinderError::new_dead(), work))
        } else {
            let sync = work.should_sync_wakeup();

            // Didn't find a thread waiting for proc work; this can happen
            // in two scenarios:
            // 1. All threads are busy handling transactions
            //    In that case, one of those threads should call back into
            //    the kernel driver soon and pick up this work.
            // 2. Threads are using the (e)poll interface, in which case
            //    they may be blocked on the waitqueue without having been
            //    added to waiting_threads. For this case, we just iterate
            //    over all threads not handling transaction work, and
            //    wake them all up. We wake all because we don't know whether
            //    a thread that called into (e)poll is handling non-binder
            //    work currently.
            self.work.push_back(work);

            // Wake up polling threads, if any.
            for thread in self.threads.values() {
                thread.notify_if_poll_ready(sync);
            }

            Ok(())
        }
    }

    /// Push work to be cancelled. Only used during process teardown.
    pub(crate) fn push_work_for_release(&mut self, work: DLArc<dyn DeliverToRead>) {
        self.work.push_back(work);
    }

    pub(crate) fn remove_node(&mut self, ptr: u64) {
        self.nodes.remove(&ptr);
    }

    /// Updates the reference count on the given node.
    pub(crate) fn update_node_refcount(
        &mut self,
        node: &DArc<Node>,
        inc: bool,
        strong: bool,
        count: usize,
        othread: Option<&Thread>,
    ) {
        let push = node.update_refcount_locked(inc, strong, count, self);

        // If we decided that we need to push work, push either to the process or to a thread if
        // one is specified.
        if let Some(node) = push {
            if let Some(thread) = othread {
                thread.push_work_deferred(node);
            } else {
                let _ = self.push_work(node);
                // Nothing to do: `push_work` may fail if the process is dead, but that's ok as in
                // that case, it doesn't care about the notification.
            }
        }
    }

    pub(crate) fn new_node_ref(
        &mut self,
        node: DArc<Node>,
        strong: bool,
        thread: Option<&Thread>,
    ) -> NodeRef {
        self.update_node_refcount(&node, true, strong, 1, thread);
        let strong_count = if strong { 1 } else { 0 };
        NodeRef::new(node, strong_count, 1 - strong_count)
    }

    pub(crate) fn new_node_ref_with_thread(
        &mut self,
        node: DArc<Node>,
        strong: bool,
        thread: &Thread,
        wrapper: Option<CritIncrWrapper>,
    ) -> Result<NodeRef, CouldNotDeliverCriticalIncrement> {
        let push = match wrapper {
            None => node
                .incr_refcount_allow_zero2one(strong, self)?
                .map(|node| node as _),
            Some(wrapper) => node.incr_refcount_allow_zero2one_with_wrapper(strong, wrapper, self),
        };
        if let Some(node) = push {
            thread.push_work_deferred(node);
        }
        let strong_count = if strong { 1 } else { 0 };
        Ok(NodeRef::new(node, strong_count, 1 - strong_count))
    }

    /// Returns an existing node with the given pointer and cookie, if one exists.
    ///
    /// Returns an error if a node with the given pointer but a different cookie exists.
    fn get_existing_node(&self, ptr: u64, cookie: u64) -> Result<Option<DArc<Node>>> {
        match self.nodes.get(&ptr) {
            None => Ok(None),
            Some(node) => {
                let (_, node_cookie) = node.get_id();
                if node_cookie == cookie {
                    Ok(Some(node.clone()))
                } else {
                    Err(EINVAL)
                }
            }
        }
    }

    fn register_thread(&mut self) -> bool {
        if self.requested_thread_count == 0 {
            return false;
        }

        self.requested_thread_count -= 1;
        self.started_thread_count += 1;
        true
    }

    /// Finds a delivered death notification with the given cookie, removes it from the thread's
    /// delivered list, and returns it.
    fn pull_delivered_death(&mut self, cookie: usize) -> Option<DArc<NodeDeath>> {
        let mut cursor_opt = self.delivered_deaths.cursor_front();
        while let Some(cursor) = cursor_opt {
            if cursor.current().cookie == cookie {
                return Some(cursor.remove().into_arc());
            }
            cursor_opt = cursor.next();
        }
        None
    }

    pub(crate) fn death_delivered(&mut self, death: DArc<NodeDeath>) {
        if let Some(death) = ListArc::try_from_arc_or_drop(death) {
            self.delivered_deaths.push_back(death);
        } else {
            pr_warn!("Notification added to `delivered_deaths` twice.");
        }
    }

    pub(crate) fn add_outstanding_txn(&mut self) {
        self.outstanding_txns += 1;
    }

    fn txns_pending_locked(&self) -> bool {
        if self.outstanding_txns > 0 {
            return true;
        }
        for thread in self.threads.values() {
            if thread.has_current_transaction() {
                return true;
            }
        }
        false
    }
}

/// Used to keep track of a node that this process has a handle to.
#[pin_data]
pub(crate) struct NodeRefInfo {
    debug_id: usize,
    /// The refcount that this process owns to the node.
    node_ref: ListArcField<NodeRef, { Self::LIST_PROC }>,
    death: ListArcField<Option<DArc<NodeDeath>>, { Self::LIST_PROC }>,
    /// Used to store this `NodeRefInfo` in the node's `refs` list.
    #[pin]
    links: ListLinks<{ Self::LIST_NODE }>,
    /// The handle for this `NodeRefInfo`.
    handle: u32,
    /// The process that has a handle to the node.
    pub(crate) process: Arc<Process>,
}

impl NodeRefInfo {
    /// The id used for the `Node::refs` list.
    pub(crate) const LIST_NODE: u64 = 0x2da16350fb724a10;
    /// The id used for the `ListArc` in `ProcessNodeRefs`.
    const LIST_PROC: u64 = 0xd703a5263dcc8650;

    fn new(node_ref: NodeRef, handle: u32, process: Arc<Process>) -> impl PinInit<Self> {
        pin_init!(Self {
            debug_id: super::next_debug_id(),
            node_ref: ListArcField::new(node_ref),
            death: ListArcField::new(None),
            links <- ListLinks::new(),
            handle,
            process,
        })
    }

    kernel::list::define_list_arc_field_getter! {
        pub(crate) fn death(&mut self<{Self::LIST_PROC}>) -> &mut Option<DArc<NodeDeath>> { death }
        pub(crate) fn node_ref(&mut self<{Self::LIST_PROC}>) -> &mut NodeRef { node_ref }
        pub(crate) fn node_ref2(&self<{Self::LIST_PROC}>) -> &NodeRef { node_ref }
    }
}

kernel::list::impl_has_list_links! {
    impl HasListLinks<{Self::LIST_NODE}> for NodeRefInfo { self.links }
}
kernel::list::impl_list_arc_safe! {
    impl ListArcSafe<{Self::LIST_NODE}> for NodeRefInfo { untracked; }
    impl ListArcSafe<{Self::LIST_PROC}> for NodeRefInfo { untracked; }
}
kernel::list::impl_list_item! {
    impl ListItem<{Self::LIST_NODE}> for NodeRefInfo {
        using ListLinks;
    }
}

/// Keeps track of references this process has to nodes owned by other processes.
///
/// TODO: Currently, the rbtree requires two allocations per node reference, and two tree
/// traversals to look up a node by `Node::global_id`. Once the rbtree is more powerful, these
/// extra costs should be eliminated.
struct ProcessNodeRefs {
    /// Used to look up nodes using the 32-bit id that this process knows it by.
    by_handle: RBTree<u32, ListArc<NodeRefInfo, { NodeRefInfo::LIST_PROC }>>,
    /// Used to look up nodes without knowing their local 32-bit id. The usize is the address of
    /// the underlying `Node` struct as returned by `Node::global_id`.
    by_node: RBTree<usize, u32>,
}

impl ProcessNodeRefs {
    fn new() -> Self {
        Self {
            by_handle: RBTree::new(),
            by_node: RBTree::new(),
        }
    }
}

/// A process using binder.
///
/// Strictly speaking, there can be multiple of these per process. There is one for each binder fd
/// that a process has opened, so processes using several binder contexts have several `Process`
/// objects. This ensures that the contexts are fully separated.
#[pin_data]
pub(crate) struct Process {
    pub(crate) ctx: Arc<Context>,

    // The task leader (process).
    pub(crate) task: ARef<Task>,

    // Credential associated with file when `Process` is created.
    pub(crate) cred: ARef<Credential>,

    #[pin]
    pub(crate) inner: SpinLock<ProcessInner>,

    pub(crate) default_priority: BinderPriority,

    #[pin]
    pub(crate) pages: ShrinkablePageRange,

    // Waitqueue of processes waiting for all outstanding transactions to be
    // processed.
    #[pin]
    freeze_wait: CondVar,

    // Node references are in a different lock to avoid recursive acquisition when
    // incrementing/decrementing a node in another process.
    #[pin]
    node_refs: Mutex<ProcessNodeRefs>,

    // Work node for deferred work item.
    #[pin]
    defer_work: Work<Process>,

    // Links for process list in Context.
    #[pin]
    links: ListLinks,
}

kernel::impl_has_work! {
    impl HasWork<Process> for Process { self.defer_work }
}

kernel::list::impl_has_list_links! {
    impl HasListLinks<0> for Process { self.links }
}
kernel::list::impl_list_arc_safe! {
    impl ListArcSafe<0> for Process { untracked; }
}
kernel::list::impl_list_item! {
    impl ListItem<0> for Process {
        using ListLinks;
    }
}

impl workqueue::WorkItem for Process {
    type Pointer = Arc<Process>;

    fn run(me: Arc<Self>) {
        let defer;
        {
            let mut inner = me.inner.lock();
            defer = inner.defer_work;
            inner.defer_work = 0;
        }

        if defer & PROC_DEFER_FLUSH != 0 {
            me.deferred_flush();
        }
        if defer & PROC_DEFER_RELEASE != 0 {
            me.deferred_release();
        }
    }
}

impl Process {
    fn new(ctx: Arc<Context>, cred: ARef<Credential>) -> Result<Arc<Self>> {
        let current = kernel::current!();
        let list_process = ListArc::pin_init(try_pin_init!(Process {
            ctx,
            cred,
            default_priority: prio::get_default_prio_from_task(current),
            inner <- kernel::new_spinlock!(ProcessInner::new(), "Process::inner"),
            pages <- ShrinkablePageRange::new(&super::BINDER_SHRINKER),
            node_refs <- kernel::new_mutex!(ProcessNodeRefs::new(), "Process::node_refs"),
            freeze_wait <- kernel::new_condvar!("Process::freeze_wait"),
            task: current.group_leader().into(),
            defer_work <- kernel::new_work!("Process::defer_work"),
            links <- ListLinks::new(),
        }))?;

        let process = list_process.clone_arc();
        process.ctx.register_process(list_process);

        Ok(process)
    }

    #[inline(never)]
    pub(crate) fn debug_print(&self, m: &mut SeqFile, ctx: &Context) -> Result<()> {
        seq_print!(m, "proc {}\n", self.task.pid_in_current_ns());
        seq_print!(m, "context {}\n", &*ctx.name);

        let mut all_threads = Vec::new();
        let mut all_nodes = Vec::new();
        loop {
            let inner = self.inner.lock();
            let num_threads = inner.threads.iter().count();
            let num_nodes = inner.nodes.iter().count();

            if all_threads.capacity() < num_threads || all_nodes.capacity() < num_nodes {
                drop(inner);
                all_threads.try_reserve(num_threads)?;
                all_nodes.try_reserve(num_nodes)?;
                continue;
            }

            for thread in inner.threads.values() {
                assert!(all_threads.len() < all_threads.capacity());
                let _ = all_threads.try_push(thread.clone());
            }

            for node in inner.nodes.values() {
                assert!(all_nodes.len() < all_nodes.capacity());
                let _ = all_nodes.try_push(node.clone());
            }

            break;
        }

        for thread in all_threads {
            thread.debug_print(m);
        }

        let mut inner = self.inner.lock();
        for node in all_nodes {
            node.full_debug_print(m, &mut inner)?;
        }
        drop(inner);

        let mut refs = self.node_refs.lock();
        for r in refs.by_handle.values_mut() {
            let node_ref = r.node_ref();
            let dead = node_ref.node.owner.inner.lock().is_dead;
            let (strong, weak) = node_ref.get_count();
            let debug_id = node_ref.node.debug_id;

            seq_print!(
                m,
                "  ref {}: desc {} {}node {debug_id} s {strong} w {weak}",
                r.debug_id,
                r.handle,
                if dead { "dead " } else { "" },
            );
        }
        drop(refs);

        let inner = self.inner.lock();
        for work in &inner.work {
            work.debug_print(m, "  ", "  pending transaction")?;
        }
        for _death in &inner.delivered_deaths {
            seq_print!(m, "  has delivered dead binder\n");
        }
        if let Some(mapping) = &inner.mapping {
            mapping.alloc.debug_print(m)?;
        }
        drop(inner);

        Ok(())
    }

    /// Attempts to fetch a work item from the process queue.
    pub(crate) fn get_work(&self) -> Option<DLArc<dyn DeliverToRead>> {
        self.inner.lock().work.pop_front()
    }

    /// Attempts to fetch a work item from the process queue. If none is available, it registers the
    /// given thread as ready to receive work directly.
    ///
    /// This must only be called when the thread is not participating in a transaction chain; when
    /// it is, work will always be delivered directly to the thread (and not through the process
    /// queue).
    pub(crate) fn get_work_or_register<'a>(
        &'a self,
        thread: &'a Arc<Thread>,
    ) -> Either<DLArc<dyn DeliverToRead>, Registration<'a>> {
        let mut inner = self.inner.lock();
        // Try to get work from the process queue.
        if let Some(work) = inner.work.pop_front() {
            return Either::Left(work);
        }

        // Register the thread as ready.
        Either::Right(Registration::new(thread, &mut inner))
    }

    fn get_current_thread(self: ArcBorrow<'_, Self>) -> Result<Arc<Thread>> {
        let id = {
            let current = kernel::current!();
            if !core::ptr::eq(current.group_leader(), &*self.task) {
                pr_err!("get_current_thread was called from the wrong process.");
                return Err(EINVAL);
            }
            current.pid()
        };

        {
            let inner = self.inner.lock();
            if let Some(thread) = inner.threads.get(&id) {
                return Ok(thread.clone());
            }
        }

        // Allocate a new `Thread` without holding any locks.
        let reservation = RBTree::try_reserve_node()?;
        let ta: Arc<Thread> = Thread::new(id, self.into())?;

        let mut inner = self.inner.lock();
        match inner.threads.entry(id) {
            rbtree::Entry::Vacant(entry) => {
                entry.insert(ta.clone(), reservation);
                Ok(ta)
            }
            rbtree::Entry::Occupied(_entry) => {
                pr_err!("Cannot create two threads with the same id.");
                Err(EINVAL)
            }
        }
    }

    pub(crate) fn push_work(&self, work: DLArc<dyn DeliverToRead>) -> BinderResult {
        // If push_work fails, drop the work item outside the lock.
        let res = self.inner.lock().push_work(work);
        match res {
            Ok(()) => Ok(()),
            Err((err, work)) => {
                drop(work);
                Err(err)
            }
        }
    }

    fn set_as_manager(
        self: ArcBorrow<'_, Self>,
        info: Option<FlatBinderObject>,
        thread: &Thread,
    ) -> Result {
        let (ptr, cookie, flags) = if let Some(obj) = info {
            (
                // SAFETY: The object type for this ioctl is implicitly `BINDER_TYPE_BINDER`, so it
                // is safe to access the `binder` field.
                unsafe { obj.__bindgen_anon_1.binder },
                obj.cookie,
                obj.flags,
            )
        } else {
            (0, 0, 0)
        };
        let node_ref = self.get_node(ptr, cookie, flags as _, true, thread)?;
        let node = node_ref.node.clone();
        self.ctx.set_manager_node(node_ref)?;
        self.inner.lock().is_manager = true;

        // Force the state of the node to prevent the delivery of acquire/increfs.
        let mut owner_inner = node.owner.inner.lock();
        node.force_has_count(&mut owner_inner);
        Ok(())
    }

    fn get_node_inner(
        self: ArcBorrow<'_, Self>,
        ptr: u64,
        cookie: u64,
        flags: u32,
        strong: bool,
        thread: &Thread,
        wrapper: Option<CritIncrWrapper>,
    ) -> Result<Result<NodeRef, CouldNotDeliverCriticalIncrement>> {
        // Try to find an existing node.
        {
            let mut inner = self.inner.lock();
            if let Some(node) = inner.get_existing_node(ptr, cookie)? {
                return Ok(inner.new_node_ref_with_thread(node, strong, thread, wrapper));
            }
        }

        // Allocate the node before reacquiring the lock.
        let node = DTRWrap::arc_pin_init(Node::new(ptr, cookie, flags, self.into()))?.into_arc();
        let rbnode = RBTree::try_allocate_node(ptr, node.clone())?;
        let mut inner = self.inner.lock();
        if let Some(node) = inner.get_existing_node(ptr, cookie)? {
            return Ok(inner.new_node_ref_with_thread(node, strong, thread, wrapper));
        }

        inner.nodes.insert(rbnode);
        // This can only fail if someone has already pushed the node to a list, but we just created
        // it and still hold the lock, so it can't fail right now.
        let node_ref = inner
            .new_node_ref_with_thread(node, strong, thread, wrapper)
            .unwrap();

        Ok(Ok(node_ref))
    }

    pub(crate) fn get_node(
        self: ArcBorrow<'_, Self>,
        ptr: u64,
        cookie: u64,
        flags: u32,
        strong: bool,
        thread: &Thread,
    ) -> Result<NodeRef> {
        let mut wrapper = None;
        for _ in 0..2 {
            match self.get_node_inner(ptr, cookie, flags, strong, thread, wrapper) {
                Err(err) => return Err(err),
                Ok(Ok(node_ref)) => return Ok(node_ref),
                Ok(Err(CouldNotDeliverCriticalIncrement)) => {
                    wrapper = Some(CritIncrWrapper::new()?);
                }
            }
        }
        // We only get a `CouldNotDeliverCriticalIncrement` error if `wrapper` is `None`, so the
        // loop should run at most twice.
        unreachable!()
    }

    pub(crate) fn insert_or_update_handle(
        self: ArcBorrow<'_, Process>,
        node_ref: NodeRef,
        is_mananger: bool,
    ) -> Result<u32> {
        {
            let mut refs = self.node_refs.lock();

            // Do a lookup before inserting.
            if let Some(handle_ref) = refs.by_node.get(&node_ref.node.global_id()) {
                let handle = *handle_ref;
                let info = refs.by_handle.get_mut(&handle).unwrap();
                info.node_ref().absorb(node_ref);
                return Ok(handle);
            }
        }

        // Reserve memory for tree nodes.
        let reserve1 = RBTree::try_reserve_node()?;
        let reserve2 = RBTree::try_reserve_node()?;
        let info = UniqueArc::try_new_uninit()?;

        let mut refs = self.node_refs.lock();

        // Do a lookup again as node may have been inserted before the lock was reacquired.
        if let Some(handle_ref) = refs.by_node.get(&node_ref.node.global_id()) {
            let handle = *handle_ref;
            let info = refs.by_handle.get_mut(&handle).unwrap();
            info.node_ref().absorb(node_ref);
            return Ok(handle);
        }

        // Find id.
        let mut target: u32 = if is_mananger { 0 } else { 1 };
        for handle in refs.by_handle.keys() {
            if *handle > target {
                break;
            }
            if *handle == target {
                target = target.checked_add(1).ok_or(ENOMEM)?;
            }
        }

        let gid = node_ref.node.global_id();
        let (info_proc, info_node) = {
            let info_init = NodeRefInfo::new(node_ref, target, self.into());
            match info.pin_init_with(info_init) {
                Ok(info) => ListArc::pair_from_pin_unique(info),
                // error is infallible
                Err(err) => match err {},
            }
        };

        // Ensure the process is still alive while we insert a new reference.
        //
        // This releases the lock before inserting the nodes, but since `is_dead` is set as the
        // first thing in `deferred_release`, process cleanup will not miss the items inserted into
        // `refs` below.
        if self.inner.lock().is_dead {
            return Err(ESRCH);
        }

        // SAFETY: `info_proc` and `info_node` reference the same node, so we are inserting
        // `info_node` into the right node's `refs` list.
        unsafe { info_proc.node_ref2().node.insert_node_info(info_node) };

        refs.by_node.insert(reserve1.into_node(gid, target));
        refs.by_handle.insert(reserve2.into_node(target, info_proc));
        Ok(target)
    }

    pub(crate) fn get_transaction_node(&self, handle: u32) -> BinderResult<NodeRef> {
        // When handle is zero, try to get the context manager.
        if handle == 0 {
            Ok(self.ctx.get_manager_node(true)?)
        } else {
            Ok(self.get_node_from_handle(handle, true)?)
        }
    }

    pub(crate) fn get_node_from_handle(&self, handle: u32, strong: bool) -> Result<NodeRef> {
        self.node_refs
            .lock()
            .by_handle
            .get_mut(&handle)
            .ok_or(ENOENT)?
            .node_ref()
            .clone(strong)
    }

    pub(crate) fn remove_from_delivered_deaths(&self, death: &DArc<NodeDeath>) {
        let mut inner = self.inner.lock();
        // SAFETY: By the invariant on the `delivered_links` field, this is the right linked list.
        let removed = unsafe { inner.delivered_deaths.remove(death) };
        drop(inner);
        drop(removed);
    }

    pub(crate) fn update_ref(
        self: ArcBorrow<'_, Process>,
        handle: u32,
        inc: bool,
        strong: bool,
    ) -> Result {
        if inc && handle == 0 {
            if let Ok(node_ref) = self.ctx.get_manager_node(strong) {
                if core::ptr::eq(&*self, &*node_ref.node.owner) {
                    return Err(EINVAL);
                }
                let _ = self.insert_or_update_handle(node_ref, true);
                return Ok(());
            }
        }

        // To preserve original binder behaviour, we only fail requests where the manager tries to
        // increment references on itself.
        let mut refs = self.node_refs.lock();
        if let Some(info) = refs.by_handle.get_mut(&handle) {
            if info.node_ref().update(inc, strong) {
                // Clean up death if there is one attached to this node reference.
                if let Some(death) = info.death().take() {
                    death.set_cleared(true);
                    self.remove_from_delivered_deaths(&death);
                }

                // Remove reference from process tables, and from the node's `refs` list.

                // SAFETY: We are removing the `NodeRefInfo` from the right node.
                unsafe { info.node_ref2().node.remove_node_info(&info) };

                let id = info.node_ref().node.global_id();
                refs.by_handle.remove(&handle);
                refs.by_node.remove(&id);
            }
        }
        Ok(())
    }

    /// Decrements the refcount of the given node, if one exists.
    pub(crate) fn update_node(&self, ptr: u64, cookie: u64, strong: bool) {
        let mut inner = self.inner.lock();
        if let Ok(Some(node)) = inner.get_existing_node(ptr, cookie) {
            inner.update_node_refcount(&node, false, strong, 1, None);
        }
    }

    pub(crate) fn inc_ref_done(&self, reader: &mut UserSliceReader, strong: bool) -> Result {
        let ptr = reader.read::<u64>()?;
        let cookie = reader.read::<u64>()?;
        let mut inner = self.inner.lock();
        if let Ok(Some(node)) = inner.get_existing_node(ptr, cookie) {
            if let Some(node) = node.inc_ref_done_locked(strong, &mut inner) {
                // This only fails if the process is dead.
                let _ = inner.push_work(node);
            }
        }
        Ok(())
    }

    pub(crate) fn buffer_alloc(
        self: &Arc<Self>,
        size: usize,
        is_oneway: bool,
        from_pid: i32,
    ) -> BinderResult<Allocation> {
        use kernel::page::PAGE_SIZE;

        let alloc = range_alloc::ReserveNewBox::try_new()?;
        let mut inner = self.inner.lock();
        let mapping = inner.mapping.as_mut().ok_or_else(BinderError::new_dead)?;
        let offset = mapping
            .alloc
            .reserve_new(size, is_oneway, from_pid, alloc)?;

        let res = Allocation::new(
            self.clone(),
            offset,
            size,
            mapping.address + offset,
            mapping.alloc.oneway_spam_detected,
        );
        drop(inner);

        // This allocation will be marked as in use until the `Allocation` is used to free it.
        //
        // This method can't be called while holding a lock, so we release the lock first. It's
        // okay for several threads to use the method on the same index at the same time. In that
        // case, one of the calls will allocate the given page (if missing), and the other call
        // will wait for the other call to finish allocating the page.
        //
        // We will not call `stop_using_range` in parallel with this on the same page, because the
        // allocation can only be removed via the destructor of the `Allocation` object that we
        // currently own.
        match self.pages.use_range(
            offset / PAGE_SIZE,
            (offset + size + (PAGE_SIZE - 1)) / PAGE_SIZE,
        ) {
            Ok(()) => {}
            Err(err) => {
                pr_warn!("use_range failure {:?}", err);
                return Err(err.into());
            }
        }

        Ok(res)
    }

    pub(crate) fn buffer_get(self: &Arc<Self>, ptr: usize) -> Option<Allocation> {
        let mut inner = self.inner.lock();
        let mapping = inner.mapping.as_mut()?;
        let offset = ptr.checked_sub(mapping.address)?;
        let (size, odata) = mapping.alloc.reserve_existing(offset).ok()?;
        let mut alloc = Allocation::new(
            self.clone(),
            offset,
            size,
            ptr,
            mapping.alloc.oneway_spam_detected,
        );
        if let Some(data) = odata {
            alloc.set_info(data);
        }
        Some(alloc)
    }

    pub(crate) fn buffer_raw_free(&self, ptr: usize) {
        let mut inner = self.inner.lock();
        if let Some(ref mut mapping) = &mut inner.mapping {
            let offset = match ptr.checked_sub(mapping.address) {
                Some(offset) => offset,
                None => return,
            };

            let freed_range = match mapping.alloc.reservation_abort(offset) {
                Ok(freed_range) => freed_range,
                Err(_) => {
                    pr_warn!(
                        "Pointer {:x} failed to free, base = {:x}\n",
                        ptr,
                        mapping.address
                    );
                    return;
                }
            };

            // No more allocations in this range. Mark them as not in use.
            //
            // Must be done before we release the lock so that `use_range` is not used on these
            // indices until `stop_using_range` returns.
            self.pages
                .stop_using_range(freed_range.start_page_idx, freed_range.end_page_idx);
        }
    }

    pub(crate) fn buffer_make_freeable(&self, offset: usize, data: Option<AllocationInfo>) {
        let mut inner = self.inner.lock();
        if let Some(ref mut mapping) = &mut inner.mapping {
            if mapping.alloc.reservation_commit(offset, data).is_err() {
                pr_warn!("Offset {} failed to be marked freeable\n", offset);
            }
        }
    }

    fn create_mapping(&self, vma: &mut mm::virt::Area) -> Result {
        use kernel::page::PAGE_SIZE;
        let size = usize::min(vma.end() - vma.start(), bindings::SZ_4M as usize);
        let mapping = Mapping::new(vma.start(), size)?;
        let page_count = self.pages.register_with_vma(vma)?;
        if page_count * PAGE_SIZE != size {
            return Err(EINVAL);
        }

        // Save range allocator for later.
        self.inner.lock().mapping = Some(mapping);

        Ok(())
    }

    fn version(&self, data: UserSlice) -> Result {
        data.writer().write(&BinderVersion::current())
    }

    pub(crate) fn register_thread(&self) -> bool {
        self.inner.lock().register_thread()
    }

    fn remove_thread(&self, thread: Arc<Thread>) {
        self.inner.lock().threads.remove(&thread.id);
        thread.release();
    }

    fn set_max_threads(&self, max: u32) {
        self.inner.lock().max_threads = max;
    }

    fn set_oneway_spam_detection_enabled(&self, enabled: u32) {
        self.inner.lock().oneway_spam_detection_enabled = enabled != 0;
    }

    pub(crate) fn is_oneway_spam_detection_enabled(&self) -> bool {
        self.inner.lock().oneway_spam_detection_enabled
    }

    fn get_node_debug_info(&self, data: UserSlice) -> Result {
        let (mut reader, mut writer) = data.reader_writer();

        // Read the starting point.
        let ptr = reader.read::<BinderNodeDebugInfo>()?.ptr;
        let mut out = BinderNodeDebugInfo::default();

        {
            let inner = self.inner.lock();
            for (node_ptr, node) in &inner.nodes {
                if *node_ptr > ptr {
                    node.populate_debug_info(&mut out, &inner);
                    break;
                }
            }
        }

        writer.write(&out)
    }

    fn get_node_info_from_ref(&self, data: UserSlice) -> Result {
        let (mut reader, mut writer) = data.reader_writer();
        let mut out = reader.read::<BinderNodeInfoForRef>()?;

        if out.strong_count != 0
            || out.weak_count != 0
            || out.reserved1 != 0
            || out.reserved2 != 0
            || out.reserved3 != 0
        {
            return Err(EINVAL);
        }

        // Only the context manager is allowed to use this ioctl.
        if !self.inner.lock().is_manager {
            return Err(EPERM);
        }

        let node_ref = self
            .get_node_from_handle(out.handle, true)
            .or(Err(EINVAL))?;
        // Get the counts from the node.
        {
            let owner_inner = node_ref.node.owner.inner.lock();
            node_ref.node.populate_counts(&mut out, &owner_inner);
        }

        // Write the result back.
        writer.write(&out)
    }

    pub(crate) fn needs_thread(&self) -> bool {
        let mut inner = self.inner.lock();
        let ret = inner.requested_thread_count == 0
            && inner.ready_threads.is_empty()
            && inner.started_thread_count < inner.max_threads;
        if ret {
            inner.requested_thread_count += 1
        }
        ret
    }

    pub(crate) fn request_death(
        self: &Arc<Self>,
        reader: &mut UserSliceReader,
        thread: &Thread,
    ) -> Result {
        let handle: u32 = reader.read()?;
        let cookie: usize = reader.read()?;

        // TODO: First two should result in error, but not the others.

        // TODO: Do we care about the context manager dying?

        // Queue BR_ERROR if we can't allocate memory for the death notification.
        let death = UniqueArc::try_new_uninit().map_err(|err| {
            thread.push_return_work(BR_ERROR);
            err
        })?;
        let mut refs = self.node_refs.lock();
        let info = refs.by_handle.get_mut(&handle).ok_or(EINVAL)?;

        // Nothing to do if there is already a death notification request for this handle.
        if info.death().is_some() {
            return Ok(());
        }

        let death = {
            let death_init = NodeDeath::new(info.node_ref().node.clone(), self.clone(), cookie);
            match death.pin_init_with(death_init) {
                Ok(death) => death,
                // error is infallible
                Err(err) => match err {},
            }
        };

        // Register the death notification.
        {
            let owner = info.node_ref2().node.owner.clone();
            let mut owner_inner = owner.inner.lock();
            if owner_inner.is_dead {
                let death = ListArc::from_pin_unique(death);
                *info.death() = Some(death.clone_arc());
                drop(owner_inner);
                let _ = self.push_work(death);
            } else {
                let death = ListArc::from_pin_unique(death);
                *info.death() = Some(death.clone_arc());
                info.node_ref().node.add_death(death, &mut owner_inner);
            }
        }
        Ok(())
    }

    pub(crate) fn clear_death(&self, reader: &mut UserSliceReader, thread: &Thread) -> Result {
        let handle: u32 = reader.read()?;
        let cookie: usize = reader.read()?;

        let mut refs = self.node_refs.lock();
        let info = refs.by_handle.get_mut(&handle).ok_or(EINVAL)?;

        let death = info.death().take().ok_or(EINVAL)?;
        if death.cookie != cookie {
            *info.death() = Some(death);
            return Err(EINVAL);
        }

        // Update state and determine if we need to queue a work item. We only need to do it when
        // the node is not dead or if the user already completed the death notification.
        if death.set_cleared(false) {
            if let Some(death) = ListArc::try_from_arc_or_drop(death) {
                let _ = thread.push_work_if_looper(death);
            }
        }

        Ok(())
    }

    pub(crate) fn dead_binder_done(&self, cookie: usize, thread: &Thread) {
        if let Some(death) = self.inner.lock().pull_delivered_death(cookie) {
            death.set_notification_done(thread);
        }
    }

    fn deferred_flush(&self) {
        let inner = self.inner.lock();
        for thread in inner.threads.values() {
            thread.exit_looper();
        }
    }

    fn deferred_release(self: Arc<Self>) {
        let is_manager = {
            let mut inner = self.inner.lock();
            inner.is_dead = true;
            inner.is_frozen = false;
            inner.sync_recv = false;
            inner.async_recv = false;
            inner.is_manager
        };

        if is_manager {
            self.ctx.unset_manager_node();
        }

        self.ctx.deregister_process(&self);

        // Move oneway_todo into the process todolist.
        {
            let mut inner = self.inner.lock();
            let nodes = take(&mut inner.nodes);
            for node in nodes.values() {
                node.release(&mut inner);
            }
            inner.nodes = nodes;
        }

        // Cancel all pending work items.
        while let Some(work) = self.get_work() {
            work.into_arc().cancel();
        }

        // Free any resources kept alive by allocated buffers.
        let omapping = self.inner.lock().mapping.take();
        if let Some(mut mapping) = omapping {
            let address = mapping.address;
            let oneway_spam_detected = mapping.alloc.oneway_spam_detected;
            mapping.alloc.take_for_each(|offset, size, odata| {
                let ptr = offset + address;
                let mut alloc =
                    Allocation::new(self.clone(), offset, size, ptr, oneway_spam_detected);
                if let Some(data) = odata {
                    alloc.set_info(data);
                }
                drop(alloc)
            });
        }

        // Drop all references. We do this dance with `swap` to avoid destroying the references
        // while holding the lock.
        let mut refs = self.node_refs.lock();
        let mut node_refs = take(&mut refs.by_handle);
        drop(refs);
        for info in node_refs.values_mut() {
            // SAFETY: We are removing the `NodeRefInfo` from the right node.
            unsafe { info.node_ref2().node.remove_node_info(&info) };

            // Remove all death notifications from the nodes (that belong to a different process).
            let death = if let Some(existing) = info.death().take() {
                existing
            } else {
                continue;
            };
            death.set_cleared(false);
        }
        drop(node_refs);

        // Do similar dance for the state lock.
        let mut inner = self.inner.lock();
        let threads = take(&mut inner.threads);
        let nodes = take(&mut inner.nodes);
        drop(inner);

        // Release all threads.
        for thread in threads.values() {
            thread.release();
        }

        // Deliver death notifications.
        for node in nodes.values() {
            loop {
                let death = {
                    let mut inner = self.inner.lock();
                    if let Some(death) = node.next_death(&mut inner) {
                        death
                    } else {
                        break;
                    }
                };
                death.set_dead();
            }
        }
    }

    pub(crate) fn drop_outstanding_txn(&self) {
        let wake = {
            let mut inner = self.inner.lock();
            if inner.outstanding_txns == 0 {
                pr_err!("outstanding_txns underflow");
                return;
            }
            inner.outstanding_txns -= 1;
            inner.is_frozen && inner.outstanding_txns == 0
        };

        if wake {
            self.freeze_wait.notify_all();
        }
    }

    pub(crate) fn ioctl_freeze(&self, info: &BinderFreezeInfo) -> Result {
        if info.enable == 0 {
            let mut inner = self.inner.lock();
            inner.sync_recv = false;
            inner.async_recv = false;
            inner.is_frozen = false;
            return Ok(());
        }

        let mut inner = self.inner.lock();
        inner.sync_recv = false;
        inner.async_recv = false;
        inner.is_frozen = true;

        if info.timeout_ms > 0 {
            let mut jiffies = kernel::time::msecs_to_jiffies(info.timeout_ms);
            while jiffies > 0 {
                if inner.outstanding_txns == 0 {
                    break;
                }

                match self
                    .freeze_wait
                    .wait_interruptible_timeout(&mut inner, jiffies)
                {
                    CondVarTimeoutResult::Signal { .. } => {
                        inner.is_frozen = false;
                        return Err(ERESTARTSYS);
                    }
                    CondVarTimeoutResult::Woken { jiffies: remaining } => {
                        jiffies = remaining;
                    }
                    CondVarTimeoutResult::Timeout => {
                        jiffies = 0;
                    }
                }
            }
        }

        if inner.txns_pending_locked() {
            inner.is_frozen = false;
            Err(EAGAIN)
        } else {
            Ok(())
        }
    }
}

fn get_frozen_status(data: UserSlice) -> Result {
    let (mut reader, mut writer) = data.reader_writer();

    let mut info = reader.read::<BinderFrozenStatusInfo>()?;
    info.sync_recv = 0;
    info.async_recv = 0;
    let mut found = false;

    for ctx in crate::context::get_all_contexts()? {
        ctx.for_each_proc(|proc| {
            if proc.task.pid() == info.pid as _ {
                found = true;
                let inner = proc.inner.lock();
                let txns_pending = inner.txns_pending_locked();
                info.async_recv |= inner.async_recv as u32;
                info.sync_recv |= inner.sync_recv as u32;
                info.sync_recv |= (txns_pending as u32) << 1;
            }
        });
    }

    if found {
        writer.write(&info)?;
        Ok(())
    } else {
        Err(EINVAL)
    }
}

fn ioctl_freeze(reader: &mut UserSliceReader) -> Result {
    let info = reader.read::<BinderFreezeInfo>()?;

    // Very unlikely for there to be more than 3, since a process normally uses at most binder and
    // hwbinder.
    let mut procs = Vec::try_with_capacity(3)?;

    let ctxs = crate::context::get_all_contexts()?;
    for ctx in ctxs {
        for proc in ctx.get_procs_with_pid(info.pid as i32)? {
            procs.try_push(proc)?;
        }
    }

    for proc in procs {
        proc.ioctl_freeze(&info)?;
    }
    Ok(())
}

/// The ioctl handler.
impl Process {
    /// Ioctls that are write-only from the perspective of userspace.
    ///
    /// The kernel will only read from the pointer that userspace provided to us.
    fn ioctl_write_only(
        this: ArcBorrow<'_, Process>,
        _file: &File,
        cmd: u32,
        reader: &mut UserSliceReader,
    ) -> Result<i32> {
        let thread = this.get_current_thread()?;
        match cmd {
            bindings::BINDER_SET_MAX_THREADS => this.set_max_threads(reader.read()?),
            bindings::BINDER_THREAD_EXIT => this.remove_thread(thread),
            bindings::BINDER_SET_CONTEXT_MGR => this.set_as_manager(None, &thread)?,
            bindings::BINDER_SET_CONTEXT_MGR_EXT => {
                this.set_as_manager(Some(reader.read()?), &thread)?
            }
            bindings::BINDER_ENABLE_ONEWAY_SPAM_DETECTION => {
                this.set_oneway_spam_detection_enabled(reader.read()?)
            }
            bindings::BINDER_FREEZE => ioctl_freeze(reader)?,
            _ => return Err(EINVAL),
        }
        Ok(0)
    }

    /// Ioctls that are read/write from the perspective of userspace.
    ///
    /// The kernel will both read from and write to the pointer that userspace provided to us.
    fn ioctl_write_read(
        this: ArcBorrow<'_, Process>,
        file: &File,
        cmd: u32,
        data: UserSlice,
    ) -> Result<i32> {
        let thread = this.get_current_thread()?;
        let blocking = (file.flags() & file::flags::O_NONBLOCK) == 0;
        match cmd {
            bindings::BINDER_WRITE_READ => thread.write_read(data, blocking)?,
            bindings::BINDER_GET_NODE_DEBUG_INFO => this.get_node_debug_info(data)?,
            bindings::BINDER_GET_NODE_INFO_FOR_REF => this.get_node_info_from_ref(data)?,
            bindings::BINDER_VERSION => this.version(data)?,
            bindings::BINDER_GET_FROZEN_INFO => get_frozen_status(data)?,
            bindings::BINDER_GET_EXTENDED_ERROR => thread.get_extended_error(data)?,
            _ => return Err(EINVAL),
        }
        Ok(0)
    }
}

/// The file operations supported by `Process`.
impl Process {
    pub(crate) fn open(ctx: ArcBorrow<'_, Context>, file: &File) -> Result<Arc<Process>> {
        Self::new(ctx.into(), ARef::from(file.cred()))
    }

    pub(crate) fn release(this: Arc<Process>, _file: &File) {
        let should_schedule;
        {
            let mut inner = this.inner.lock();
            should_schedule = inner.defer_work == 0;
            inner.defer_work |= PROC_DEFER_RELEASE;
        }

        if should_schedule {
            // Ignore failures to schedule to the workqueue. Those just mean that we're already
            // scheduled for execution.
            let _ = workqueue::system().enqueue(this);
        }
    }

    pub(crate) fn flush(this: ArcBorrow<'_, Process>) -> Result {
        let should_schedule;
        {
            let mut inner = this.inner.lock();
            should_schedule = inner.defer_work == 0;
            inner.defer_work |= PROC_DEFER_FLUSH;
        }

        if should_schedule {
            // Ignore failures to schedule to the workqueue. Those just mean that we're already
            // scheduled for execution.
            let _ = workqueue::system().enqueue(Arc::from(this));
        }
        Ok(())
    }

    pub(crate) fn ioctl(
        this: ArcBorrow<'_, Process>,
        file: &File,
        cmd: u32,
        arg: *mut core::ffi::c_void,
    ) -> Result<i32> {
        use kernel::ioctl::{_IOC_DIR, _IOC_SIZE};
        use kernel::uapi::{_IOC_READ, _IOC_WRITE};

        let user_slice = UserSlice::new(arg, _IOC_SIZE(cmd));

        const _IOC_READ_WRITE: u32 = _IOC_READ | _IOC_WRITE;

        match _IOC_DIR(cmd) {
            _IOC_WRITE => Self::ioctl_write_only(this, file, cmd, &mut user_slice.reader()),
            _IOC_READ_WRITE => Self::ioctl_write_read(this, file, cmd, user_slice),
            _ => Err(EINVAL),
        }
    }

    pub(crate) fn compat_ioctl(
        this: ArcBorrow<'_, Process>,
        file: &File,
        cmd: u32,
        arg: *mut core::ffi::c_void,
    ) -> Result<i32> {
        Self::ioctl(this, file, cmd, arg)
    }

    pub(crate) fn mmap(
        this: ArcBorrow<'_, Process>,
        _file: &File,
        vma: &mut mm::virt::Area,
    ) -> Result {
        // We don't allow mmap to be used in a different process.
        if !core::ptr::eq(kernel::current!().group_leader(), &*this.task) {
            return Err(EINVAL);
        }
        if vma.start() == 0 {
            return Err(EINVAL);
        }
        let mut flags = vma.flags();
        use mm::virt::flags::*;
        if flags & WRITE != 0 {
            return Err(EPERM);
        }
        flags |= DONTCOPY | MIXEDMAP;
        flags &= !MAYWRITE;
        vma.set_flags(flags);
        // TODO: Set ops. We need to learn when the user unmaps so that we can stop using it.
        this.create_mapping(vma)
    }

    pub(crate) fn poll(
        this: ArcBorrow<'_, Process>,
        file: &File,
        table: &mut PollTable,
    ) -> Result<u32> {
        let thread = this.get_current_thread()?;
        let (from_proc, mut mask) = thread.poll(file, table);
        if mask == 0 && from_proc && !this.inner.lock().work.is_empty() {
            mask |= bindings::POLLIN;
        }
        Ok(mask)
    }
}

/// Represents that a thread has registered with the `ready_threads` list of its process.
///
/// The destructor of this type will unregister the thread from the list of ready threads.
pub(crate) struct Registration<'a> {
    thread: &'a Arc<Thread>,
}

impl<'a> Registration<'a> {
    fn new(
        thread: &'a Arc<Thread>,
        guard: &mut Guard<'_, ProcessInner, kernel::sync::lock::spinlock::SpinLockBackend>,
    ) -> Self {
        assert!(core::ptr::eq(&thread.process.inner, guard.lock()));
        // INVARIANT: We are pushing this thread to the right `ready_threads` list.
        if let Ok(list_arc) = ListArc::try_from_arc(thread.clone()) {
            guard.ready_threads.push_front(list_arc);
        } else {
            // It is an error to hit this branch, and it should not be reachable. We try to do
            // something reasonable when the failure path happens. Most likely, the thread in
            // question will sleep forever.
            pr_err!("Same thread registered with `ready_threads` twice.");
        }
        Self { thread }
    }
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        let mut inner = self.thread.process.inner.lock();
        // SAFETY: The thread has the invariant that we never push it to any other linked list than
        // the `ready_threads` list of its parent process. Therefore, the thread is either in that
        // list, or in no list.
        unsafe { inner.ready_threads.remove(self.thread) };
    }
}
