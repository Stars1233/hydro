use std::cell::RefCell;
use std::marker::PhantomData;
use std::ops::Deref;
use std::rc::Rc;

use stageleft::{q, IntoQuotedMut, QuotedWithContext};

use crate::builder::FLOW_USED_MESSAGE;
use crate::cycle::{
    CycleCollection, CycleCollectionWithInitial, CycleComplete, DeferTick, ForwardRefMarker,
    TickCycleMarker,
};
use crate::ir::{HydroLeaf, HydroNode, TeeNode};
use crate::location::tick::{NoTimestamp, Timestamped};
use crate::location::{check_matching_location, Location, LocationId, NoTick, Tick};
use crate::{Bounded, Optional, Stream, Unbounded};

pub struct Singleton<T, L, B> {
    pub(crate) location: L,
    pub(crate) ir_node: RefCell<HydroNode>,

    _phantom: PhantomData<(T, L, B)>,
}

impl<'a, T, L: Location<'a>, B> Singleton<T, L, B> {
    pub(crate) fn new(location: L, ir_node: HydroNode) -> Self {
        Singleton {
            location,
            ir_node: RefCell::new(ir_node),
            _phantom: PhantomData,
        }
    }

    fn location_kind(&self) -> LocationId {
        self.location.id()
    }
}

impl<'a, T, L: Location<'a>> From<Singleton<T, L, Bounded>> for Singleton<T, L, Unbounded> {
    fn from(singleton: Singleton<T, L, Bounded>) -> Self {
        Singleton::new(singleton.location, singleton.ir_node.into_inner())
    }
}

impl<'a, T, L: Location<'a>> DeferTick for Singleton<T, Tick<L>, Bounded> {
    fn defer_tick(self) -> Self {
        Singleton::defer_tick(self)
    }
}

impl<'a, T, L: Location<'a>> CycleCollectionWithInitial<'a, TickCycleMarker>
    for Singleton<T, Tick<L>, Bounded>
{
    type Location = Tick<L>;

    fn create_source(ident: syn::Ident, initial: Self, location: Tick<L>) -> Self {
        let location_id = location.id();
        Singleton::new(
            location,
            HydroNode::Chain(
                Box::new(HydroNode::CycleSource {
                    ident,
                    location_kind: location_id,
                }),
                initial.ir_node.into_inner().into(),
            ),
        )
    }
}

impl<'a, T, L: Location<'a>> CycleComplete<'a, TickCycleMarker> for Singleton<T, Tick<L>, Bounded> {
    fn complete(self, ident: syn::Ident, expected_location: LocationId) {
        assert_eq!(
            self.location.id(),
            expected_location,
            "locations do not match"
        );
        self.location
            .flow_state()
            .borrow_mut()
            .leaves
            .as_mut()
            .expect(FLOW_USED_MESSAGE)
            .push(HydroLeaf::CycleSink {
                ident,
                location_kind: self.location_kind(),
                input: Box::new(self.ir_node.into_inner()),
            });
    }
}

impl<'a, T, L: Location<'a>> CycleCollection<'a, ForwardRefMarker>
    for Singleton<T, Tick<L>, Bounded>
{
    type Location = Tick<L>;

    fn create_source(ident: syn::Ident, location: Tick<L>) -> Self {
        let location_id = location.id();
        Singleton::new(
            location,
            HydroNode::CycleSource {
                ident,
                location_kind: location_id,
            },
        )
    }
}

impl<'a, T, L: Location<'a>> CycleComplete<'a, ForwardRefMarker>
    for Singleton<T, Tick<L>, Bounded>
{
    fn complete(self, ident: syn::Ident, expected_location: LocationId) {
        assert_eq!(
            self.location.id(),
            expected_location,
            "locations do not match"
        );
        self.location
            .flow_state()
            .borrow_mut()
            .leaves
            .as_mut()
            .expect(FLOW_USED_MESSAGE)
            .push(HydroLeaf::CycleSink {
                ident,
                location_kind: self.location_kind(),
                input: Box::new(self.ir_node.into_inner()),
            });
    }
}

impl<'a, T, L: Location<'a> + NoTick, B> CycleCollection<'a, ForwardRefMarker>
    for Singleton<T, L, B>
{
    type Location = L;

    fn create_source(ident: syn::Ident, location: L) -> Self {
        let location_id = location.id();
        Singleton::new(
            location,
            HydroNode::Persist(Box::new(HydroNode::CycleSource {
                ident,
                location_kind: location_id,
            })),
        )
    }
}

impl<'a, T, L: Location<'a> + NoTick, B> CycleComplete<'a, ForwardRefMarker>
    for Singleton<T, L, B>
{
    fn complete(self, ident: syn::Ident, expected_location: LocationId) {
        assert_eq!(
            self.location.id(),
            expected_location,
            "locations do not match"
        );
        self.location
            .flow_state()
            .borrow_mut()
            .leaves
            .as_mut()
            .expect(FLOW_USED_MESSAGE)
            .push(HydroLeaf::CycleSink {
                ident,
                location_kind: self.location_kind(),
                input: Box::new(HydroNode::Unpersist(Box::new(self.ir_node.into_inner()))),
            });
    }
}

impl<'a, T: Clone, L: Location<'a>, B> Clone for Singleton<T, L, B> {
    fn clone(&self) -> Self {
        if !matches!(self.ir_node.borrow().deref(), HydroNode::Tee { .. }) {
            let orig_ir_node = self.ir_node.replace(HydroNode::Placeholder);
            *self.ir_node.borrow_mut() = HydroNode::Tee {
                inner: TeeNode(Rc::new(RefCell::new(orig_ir_node))),
            };
        }

        if let HydroNode::Tee { inner } = self.ir_node.borrow().deref() {
            Singleton {
                location: self.location.clone(),
                ir_node: HydroNode::Tee {
                    inner: TeeNode(inner.0.clone()),
                }
                .into(),
                _phantom: PhantomData,
            }
        } else {
            unreachable!()
        }
    }
}

impl<'a, T, L: Location<'a>, B> Singleton<T, L, B> {
    pub fn map<U, F: Fn(T) -> U + 'a>(self, f: impl IntoQuotedMut<'a, F, L>) -> Singleton<U, L, B> {
        let f = f.splice_fn1_ctx(&self.location).into();
        Singleton::new(
            self.location,
            HydroNode::Map {
                f,
                input: Box::new(self.ir_node.into_inner()),
            },
        )
    }

    pub fn flat_map_ordered<U, I: IntoIterator<Item = U>, F: Fn(T) -> I + 'a>(
        self,
        f: impl IntoQuotedMut<'a, F, L>,
    ) -> Stream<U, L, B> {
        let f = f.splice_fn1_ctx(&self.location).into();
        Stream::new(
            self.location,
            HydroNode::FlatMap {
                f,
                input: Box::new(self.ir_node.into_inner()),
            },
        )
    }

    pub fn flat_map_unordered<U, I: IntoIterator<Item = U>, F: Fn(T) -> I + 'a>(
        self,
        f: impl IntoQuotedMut<'a, F, L>,
    ) -> Stream<U, L, B> {
        let f = f.splice_fn1_ctx(&self.location).into();
        Stream::new(
            self.location,
            HydroNode::FlatMap {
                f,
                input: Box::new(self.ir_node.into_inner()),
            },
        )
    }

    pub fn filter<F: Fn(&T) -> bool + 'a>(
        self,
        f: impl IntoQuotedMut<'a, F, L>,
    ) -> Optional<T, L, B> {
        let f = f.splice_fn1_borrow_ctx(&self.location).into();
        Optional::new(
            self.location,
            HydroNode::Filter {
                f,
                input: Box::new(self.ir_node.into_inner()),
            },
        )
    }

    pub fn filter_map<U, F: Fn(T) -> Option<U> + 'a>(
        self,
        f: impl IntoQuotedMut<'a, F, L>,
    ) -> Optional<U, L, B> {
        let f = f.splice_fn1_ctx(&self.location).into();
        Optional::new(
            self.location,
            HydroNode::FilterMap {
                f,
                input: Box::new(self.ir_node.into_inner()),
            },
        )
    }

    pub fn zip<Other>(self, other: Other) -> <Self as ZipResult<'a, Other>>::Out
    where
        Self: ZipResult<'a, Other, Location = L>,
    {
        check_matching_location(&self.location, &Self::other_location(&other));

        if L::is_top_level() {
            Self::make(
                self.location,
                HydroNode::Persist(Box::new(HydroNode::CrossSingleton(
                    Box::new(HydroNode::Unpersist(Box::new(self.ir_node.into_inner()))),
                    Box::new(HydroNode::Unpersist(Box::new(Self::other_ir_node(other)))),
                ))),
            )
        } else {
            Self::make(
                self.location,
                HydroNode::CrossSingleton(
                    Box::new(self.ir_node.into_inner()),
                    Box::new(Self::other_ir_node(other)),
                ),
            )
        }
    }

    pub fn continue_if<U>(self, signal: Optional<U, L, Bounded>) -> Optional<T, L, Bounded>
    where
        Self: ZipResult<
            'a,
            Optional<(), L, Bounded>,
            Location = L,
            Out = Optional<(T, ()), L, Bounded>,
        >,
    {
        self.zip(signal.map(q!(|_u| ()))).map(q!(|(d, _signal)| d))
    }

    pub fn continue_unless<U>(self, other: Optional<U, L, Bounded>) -> Optional<T, L, Bounded>
    where
        Singleton<T, L, B>: ZipResult<
            'a,
            Optional<(), L, Bounded>,
            Location = L,
            Out = Optional<(T, ()), L, Bounded>,
        >,
    {
        self.continue_if(other.into_stream().count().filter(q!(|c| *c == 0)))
    }
}

impl<'a, T, L: Location<'a> + NoTick, B> Singleton<T, Timestamped<L>, B> {
    /// Given a tick, returns a singleton value corresponding to a snapshot of the singleton
    /// as of that tick. The snapshot at tick `t + 1` is guaranteed to include at least all
    /// relevant data that contributed to the snapshot at tick `t`.
    ///
    /// # Safety
    /// Because this picks a snapshot of a singleton whose value is continuously changing,
    /// the output singleton has a non-deterministic value since the snapshot can be at an
    /// arbitrary point in time.
    pub unsafe fn latest_tick(self) -> Singleton<T, Tick<L>, Bounded> {
        Singleton::new(
            self.location.tick,
            HydroNode::Unpersist(Box::new(self.ir_node.into_inner())),
        )
    }

    pub fn drop_timestamp(self) -> Optional<T, L, B> {
        Optional::new(self.location.tick.l, self.ir_node.into_inner())
    }
}

impl<'a, T, L: Location<'a> + NoTick, B> Singleton<T, L, B> {
    pub fn timestamped(self, tick: &Tick<L>) -> Singleton<T, Timestamped<L>, B> {
        Singleton::new(
            Timestamped { tick: tick.clone() },
            self.ir_node.into_inner(),
        )
    }

    /// Eagerly samples the singleton as fast as possible, returning a stream of snapshots
    /// with order corresponding to increasing prefixes of data contributing to the singleton.
    ///
    /// # Safety
    /// At runtime, the singleton will be arbitrarily sampled as fast as possible, but due
    /// to non-deterministic batching and arrival of inputs, the output stream is
    /// non-deterministic.
    pub unsafe fn sample_eager(self) -> Stream<T, L, Unbounded> {
        let tick = self.location.tick();

        unsafe {
            // SAFETY: source of intentional non-determinism
            self.timestamped(&tick)
                .latest_tick()
                .all_ticks()
                .drop_timestamp()
        }
    }

    /// Given a time interval, returns a stream corresponding to snapshots of the singleton
    /// value taken at various points in time. Because the input singleton may be
    /// [`Unbounded`], there are no guarantees on what these snapshots are other than they
    /// represent the value of the singleton given some prefix of the streams leading up to
    /// it.
    ///
    /// # Safety
    /// The output stream is non-deterministic in which elements are sampled, since this
    /// is controlled by a clock.
    pub unsafe fn sample_every(
        self,
        interval: impl QuotedWithContext<'a, std::time::Duration, L> + Copy + 'a,
    ) -> Stream<T, L, Unbounded>
    where
        L: NoTimestamp,
    {
        let samples = unsafe {
            // SAFETY: source of intentional non-determinism
            self.location.source_interval(interval)
        };
        let tick = self.location.tick();

        unsafe {
            // SAFETY: source of intentional non-determinism
            self.timestamped(&tick)
                .latest_tick()
                .continue_if(samples.timestamped(&tick).tick_batch().first())
                .all_ticks()
                .drop_timestamp()
        }
    }
}

impl<'a, T, L: Location<'a>> Singleton<T, Tick<L>, Bounded> {
    pub fn all_ticks(self) -> Stream<T, Timestamped<L>, Unbounded> {
        Stream::new(
            Timestamped {
                tick: self.location,
            },
            HydroNode::Persist(Box::new(self.ir_node.into_inner())),
        )
    }

    pub fn latest(self) -> Singleton<T, Timestamped<L>, Unbounded> {
        Singleton::new(
            Timestamped {
                tick: self.location,
            },
            HydroNode::Persist(Box::new(self.ir_node.into_inner())),
        )
    }

    pub fn defer_tick(self) -> Singleton<T, Tick<L>, Bounded> {
        Singleton::new(
            self.location,
            HydroNode::DeferTick(Box::new(self.ir_node.into_inner())),
        )
    }

    pub fn persist(self) -> Stream<T, Tick<L>, Bounded> {
        Stream::new(
            self.location,
            HydroNode::Persist(Box::new(self.ir_node.into_inner())),
        )
    }

    pub fn delta(self) -> Optional<T, Tick<L>, Bounded> {
        Optional::new(
            self.location,
            HydroNode::Delta(Box::new(self.ir_node.into_inner())),
        )
    }

    pub fn into_stream(self) -> Stream<T, Tick<L>, Bounded> {
        Stream::new(self.location, self.ir_node.into_inner())
    }
}

pub trait ZipResult<'a, Other> {
    type Out;
    type Location;

    fn other_location(other: &Other) -> Self::Location;
    fn other_ir_node(other: Other) -> HydroNode;

    fn make(location: Self::Location, ir_node: HydroNode) -> Self::Out;
}

impl<'a, T, U: Clone, L: Location<'a>, B> ZipResult<'a, Singleton<U, Timestamped<L>, B>>
    for Singleton<T, Timestamped<L>, B>
{
    type Out = Singleton<(T, U), Timestamped<L>, B>;
    type Location = Timestamped<L>;

    fn other_location(other: &Singleton<U, Timestamped<L>, B>) -> Timestamped<L> {
        other.location.clone()
    }

    fn other_ir_node(other: Singleton<U, Timestamped<L>, B>) -> HydroNode {
        other.ir_node.into_inner()
    }

    fn make(location: Timestamped<L>, ir_node: HydroNode) -> Self::Out {
        Singleton::new(location, ir_node)
    }
}

impl<'a, T, U: Clone, L: Location<'a>, B> ZipResult<'a, Optional<U, Timestamped<L>, B>>
    for Singleton<T, Timestamped<L>, B>
{
    type Out = Optional<(T, U), Timestamped<L>, B>;
    type Location = Timestamped<L>;

    fn other_location(other: &Optional<U, Timestamped<L>, B>) -> Timestamped<L> {
        other.location.clone()
    }

    fn other_ir_node(other: Optional<U, Timestamped<L>, B>) -> HydroNode {
        other.ir_node.into_inner()
    }

    fn make(location: Timestamped<L>, ir_node: HydroNode) -> Self::Out {
        Optional::new(location, ir_node)
    }
}

impl<'a, T, U: Clone, L: Location<'a>, B> ZipResult<'a, Singleton<U, Tick<L>, B>>
    for Singleton<T, Tick<L>, B>
{
    type Out = Singleton<(T, U), Tick<L>, B>;
    type Location = Tick<L>;

    fn other_location(other: &Singleton<U, Tick<L>, B>) -> Tick<L> {
        other.location.clone()
    }

    fn other_ir_node(other: Singleton<U, Tick<L>, B>) -> HydroNode {
        other.ir_node.into_inner()
    }

    fn make(location: Tick<L>, ir_node: HydroNode) -> Self::Out {
        Singleton::new(location, ir_node)
    }
}

impl<'a, T, U: Clone, L: Location<'a>, B> ZipResult<'a, Optional<U, Tick<L>, B>>
    for Singleton<T, Tick<L>, B>
{
    type Out = Optional<(T, U), Tick<L>, B>;
    type Location = Tick<L>;

    fn other_location(other: &Optional<U, Tick<L>, B>) -> Tick<L> {
        other.location.clone()
    }

    fn other_ir_node(other: Optional<U, Tick<L>, B>) -> HydroNode {
        other.ir_node.into_inner()
    }

    fn make(location: Tick<L>, ir_node: HydroNode) -> Self::Out {
        Optional::new(location, ir_node)
    }
}
