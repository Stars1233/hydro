use hydroflow::hydroflow_syntax;

fn main() {
    let mut df = hydroflow_syntax! {
        j = join() -> for_each(std::mem::drop);
        recv_iter([(1, 1), (2, 2)]) -> j;
        recv_iter([(3, 3), (4, 4)]) -> j;
    };
    df.run_available();
}
