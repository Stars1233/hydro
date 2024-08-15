use hydroflow_plus::*;
use serde::{Deserialize, Serialize};
use stageleft::*;

#[derive(Serialize, Deserialize)]
struct SendOverNetwork {
    pub n: u32,
}

pub struct P1 {}
pub struct P2 {}

pub fn first_ten_distributed(flow: &FlowBuilder) -> (Process<P1>, Process<P2>) {
    let process = flow.process::<P1>();
    let second_process = flow.process::<P2>();

    let numbers = flow.source_iter(&process, q!(0..10));
    numbers
        .map(q!(|n| SendOverNetwork { n }))
        .send_bincode(&second_process)
        .for_each(q!(|n: SendOverNetwork| println!("{}", n.n))); // TODO(shadaj): why is the explicit type required here?

    (process, second_process)
}

use hydroflow_plus::util::cli::HydroCLI;
use hydroflow_plus_cli_integration::{CLIRuntime, HydroflowPlusMeta};

#[stageleft::entry]
pub fn first_ten_distributed_runtime<'a>(
    flow: FlowBuilder<'a>,
    cli: RuntimeData<&'a HydroCLI<HydroflowPlusMeta>>,
) -> impl Quoted<'a, Hydroflow<'a>> {
    let _ = first_ten_distributed(&flow);
    flow.with_default_optimize()
        .compile::<CLIRuntime>(&cli)
        .with_dynamic_id(q!(cli.meta.subgraph_id))
}

#[stageleft::runtime]
#[cfg(test)]
mod tests {
    use hydro_deploy::{Deployment, HydroflowCrate};
    use hydroflow_plus_cli_integration::{DeployCrateWrapper, DeployProcessSpec};

    #[tokio::test]
    async fn first_ten_distributed() {
        let mut deployment = Deployment::new();
        let localhost = deployment.Localhost();

        let builder = hydroflow_plus::FlowBuilder::new();
        let (first_node, second_node) = super::first_ten_distributed(&builder);

        let built = builder.with_default_optimize();

        insta::assert_debug_snapshot!(built.ir());

        // if we drop this, we drop the references to the deployment nodes
        let nodes = built
            .with_process(
                &first_node,
                DeployProcessSpec::new({
                    HydroflowCrate::new(".", localhost.clone())
                        .bin("first_ten_distributed")
                        .profile("dev")
                }),
            )
            .with_process(
                &second_node,
                DeployProcessSpec::new({
                    HydroflowCrate::new(".", localhost.clone())
                        .bin("first_ten_distributed")
                        .profile("dev")
                }),
            )
            .deploy(&mut deployment);

        deployment.deploy().await.unwrap();

        let mut second_node_stdout = nodes.get_process(&second_node).stdout().await;

        deployment.start().await.unwrap();

        for i in 0..10 {
            assert_eq!(second_node_stdout.recv().await.unwrap(), i.to_string());
        }
    }
}