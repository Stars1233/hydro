use quote::{quote_spanned, ToTokens};
use syn::parse_quote;

use super::{
    FlowProperties, FlowPropertyVal, OperatorCategory, OperatorConstraints, OperatorWriteOutput,
    Persistence, WriteContextArgs, RANGE_1,
};
use crate::diagnostic::{Diagnostic, Level};
use crate::graph::{OpInstGenerics, OperatorInstance};

/// > 2 input streams of type <(K, V1)> and <(K, V2)>, 1 output stream of type <(K, (V1, V2))>
///
/// Forms the equijoin of the tuples in the input streams by their first (key) attribute. Note that the result nests the 2nd input field (values) into a tuple in the 2nd output field.
///
/// ```hydroflow
/// // should print `(hello, (world, cleveland))`
/// source_iter(vec![("hello", "world"), ("stay", "gold")]) -> [0]my_join;
/// source_iter(vec![("hello", "cleveland")]) -> [1]my_join;
/// my_join = join()
///     -> assert([("hello", ("world", "cleveland"))]);
/// ```
///
/// `join` can also be provided with one or two generic lifetime persistence arguments, either
/// `'tick` or `'static`, to specify how join data persists. With `'tick`, pairs will only be
/// joined with corresponding pairs within the same tick. With `'static`, pairs will be remembered
/// across ticks and will be joined with pairs arriving in later ticks. When not explicitly
/// specified persistence defaults to `static.
///
/// When two persistence arguments are supplied the first maps to port `0` and the second maps to
/// port `1`.
/// When a single persistence argument is supplied, it is applied to both input ports.
/// When no persistence arguments are applied it defaults to `'static` for both.
///
/// The syntax is as follows:
/// ```hydroflow,ignore
/// join(); // Or
/// join::<'static>();
///
/// join::<'tick>();
///
/// join::<'static, 'tick>();
///
/// join::<'tick, 'static>();
/// // etc.
/// ```
///
/// Join also accepts one type argument that controls how the join state is built up. This (currently) allows switching between a SetUnion and NonSetUnion implementation.
/// For example:
/// ```hydroflow,ignore
/// join::<HalfSetJoinState>();
/// join::<HalfMultisetJoinState>();
/// ```
///
/// ### Examples
///
/// ```rustbook
/// let (input_send, input_recv) = hydroflow::util::unbounded_channel::<(&str, &str)>();
/// let mut flow = hydroflow::hydroflow_syntax! {
///     source_iter([("hello", "world")]) -> [0]my_join;
///     source_stream(input_recv) -> [1]my_join;
///     my_join = join::<'tick>() -> for_each(|(k, (v1, v2))| println!("({}, ({}, {}))", k, v1, v2));
/// };
/// input_send.send(("hello", "oakland")).unwrap();
/// flow.run_tick();
/// input_send.send(("hello", "san francisco")).unwrap();
/// flow.run_tick();
/// ```
/// Prints out `"(hello, (world, oakland))"` since `source_iter([("hello", "world")])` is only
/// included in the first tick, then forgotten.
///
/// ---
///
/// ```rustbook
/// let (input_send, input_recv) = hydroflow::util::unbounded_channel::<(&str, &str)>();
/// let mut flow = hydroflow::hydroflow_syntax! {
///     source_iter([("hello", "world")]) -> [0]my_join;
///     source_stream(input_recv) -> [1]my_join;
///     my_join = join::<'static>() -> for_each(|(k, (v1, v2))| println!("({}, ({}, {}))", k, v1, v2));
/// };
/// input_send.send(("hello", "oakland")).unwrap();
/// flow.run_tick();
/// input_send.send(("hello", "san francisco")).unwrap();
/// flow.run_tick();
/// ```
/// Prints out `"(hello, (world, oakland))"` and `"(hello, (world, san francisco))"` since the
/// inputs are peristed across ticks.
pub const JOIN: OperatorConstraints = OperatorConstraints {
    name: "join",
    categories: &[OperatorCategory::MultiIn],
    hard_range_inn: &(2..=2),
    soft_range_inn: &(2..=2),
    hard_range_out: RANGE_1,
    soft_range_out: RANGE_1,
    num_args: 0,
    persistence_args: &(0..=2),
    type_args: &(0..=1),
    is_external_input: false,
    ports_inn: Some(|| super::PortListSpec::Fixed(parse_quote! { 0, 1 })),
    ports_out: None,
    properties: FlowProperties {
        deterministic: FlowPropertyVal::Preserve,
        monotonic: FlowPropertyVal::Preserve,
        inconsistency_tainted: false,
    },
    input_delaytype_fn: |_| None,
    write_fn: |wc @ &WriteContextArgs {
                   root,
                   context,
                   hydroflow,
                   op_span,
                   ident,
                   inputs,
                   op_inst:
                       OperatorInstance {
                           generics:
                               OpInstGenerics {
                                   persistence_args,
                                   type_args,
                                   ..
                               },
                           ..
                       },
                   ..
               },
               diagnostics| {
        let join_type =
            type_args
                .get(0)
                .map(ToTokens::to_token_stream)
                .unwrap_or(quote_spanned!(op_span=>
                    #root::compiled::pull::HalfSetJoinState
                ));

        // TODO: This is really bad.
        // This will break if the user aliases HalfSetJoinState to something else. Temporary hacky solution.
        // Note that cross_join() depends on the implementation here as well.
        // Need to decide on what to do about multisetjoin.
        // Should it be a separate operator (multisetjoin() and multisetcrossjoin())?
        // Should the default be multiset join? And setjoin requires the use of lattice_join() with SetUnion lattice?
        let additional_trait_bounds = if join_type.to_string().contains("HalfSetJoinState") {
            quote_spanned!(op_span=>
                + ::std::cmp::Eq
            )
        } else {
            quote_spanned!(op_span=>)
        };

        let mut make_joindata = |persistence, side| {
            let joindata_ident = wc.make_ident(format!("joindata_{}", side));
            let borrow_ident = wc.make_ident(format!("joindata_{}_borrow", side));
            let (init, borrow) = match persistence {
                Persistence::Tick => (
                    quote_spanned! {op_span=>
                        #root::util::monotonic_map::MonotonicMap::new_init(
                            #join_type::default()
                        )
                    },
                    quote_spanned! {op_span=>
                        &mut *#borrow_ident.get_mut_clear(#context.current_tick())
                    },
                ),
                Persistence::Static => (
                    quote_spanned! {op_span=>
                        #join_type::default()
                    },
                    quote_spanned! {op_span=>
                        &mut *#borrow_ident
                    },
                ),
                Persistence::Mutable => {
                    diagnostics.push(Diagnostic::spanned(
                        op_span,
                        Level::Error,
                        "An implementation of 'mutable does not exist",
                    ));
                    return Err(());
                }
            };
            Ok((joindata_ident, borrow_ident, init, borrow))
        };

        let persistences = match persistence_args[..] {
            [] => [Persistence::Static, Persistence::Static],
            [a] => [a, a],
            [a, b] => [a, b],
            _ => unreachable!(),
        };

        let (lhs_joindata_ident, lhs_borrow_ident, lhs_init, lhs_borrow) =
            make_joindata(persistences[0], "lhs")?;
        let (rhs_joindata_ident, rhs_borrow_ident, rhs_init, rhs_borrow) =
            make_joindata(persistences[1], "rhs")?;

        let write_prologue = quote_spanned! {op_span=>
            let #lhs_joindata_ident = #hydroflow.add_state(std::cell::RefCell::new(
                #lhs_init
            ));
            let #rhs_joindata_ident = #hydroflow.add_state(std::cell::RefCell::new(
                #rhs_init
            ));
        };

        let lhs = &inputs[0];
        let rhs = &inputs[1];
        let write_iterator = quote_spanned! {op_span=>
            let mut #lhs_borrow_ident = #context.state_ref(#lhs_joindata_ident).borrow_mut();
            let mut #rhs_borrow_ident = #context.state_ref(#rhs_joindata_ident).borrow_mut();
            let #ident = {
                // Limit error propagation by bounding locally, erasing output iterator type.
                #[inline(always)]
                fn check_inputs<'a, K, I1, V1, I2, V2>(
                    lhs: I1,
                    rhs: I2,
                    lhs_state: &'a mut #join_type<K, V1, V2>,
                    rhs_state: &'a mut #join_type<K, V2, V1>,
                ) -> impl 'a + Iterator<Item = (K, (V1, V2))>
                where
                    K: Eq + std::hash::Hash + Clone,
                    V1: Clone #additional_trait_bounds,
                    V2: Clone #additional_trait_bounds,
                    I1: 'a + Iterator<Item = (K, V1)>,
                    I2: 'a + Iterator<Item = (K, V2)>,
                {
                    #root::compiled::pull::SymmetricHashJoin::new_from_mut(lhs, rhs, lhs_state, rhs_state)
                }
                check_inputs(#lhs, #rhs, #lhs_borrow, #rhs_borrow)
            };
        };

        Ok(OperatorWriteOutput {
            write_prologue,
            write_iterator,
            ..Default::default()
        })
    },
};
