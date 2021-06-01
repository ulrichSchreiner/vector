use indoc::indoc;
use k8s_e2e_tests::*;
use k8s_test_framework::{
    lock, test_pod, vector::Config as VectorConfig, wait_for_resource::WaitFor,
};

const HELM_CHART_VECTOR: &str = "vector";

fn helm_values_stdout_sink(aggregator_override_name: &str, agent_override_name: &str) -> String {
    format!(
        indoc! {r#"
    vector-agent:
      fullnameOverride: "{}"
      vectorSink:
        host: "{}"
      dataVolume:
        hostPath:
          path: /var/lib/{}-vector/

    vector-aggregator:
      fullnameOverride: "{}"
      vectorSource:
        sourceId: vector

      sinks:
        stdout:
          type: "console"
          inputs: ["vector"]
          target: "stdout"
          encoding: "json"
"# },
        agent_override_name,
        aggregator_override_name,
        agent_override_name,
        aggregator_override_name
    )
}

/// This test validates that vector picks up logs with an agent and
/// delivers them to the aggregator out of the box.
#[tokio::test]
async fn logs() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = lock();
    let namespace = get_namespace();
    let pod_namespace = get_namespace_appended("test-pod");
    let framework = make_framework();
    let aggregator_override_name = get_override_name("vector-aggregator");
    let agent_override_name = get_override_name("vector-agent");

    let vector = framework
        .vector(
            &namespace,
            HELM_CHART_VECTOR,
            VectorConfig {
                custom_helm_values: &helm_values_stdout_sink(
                    &aggregator_override_name,
                    &agent_override_name,
                ),
                ..Default::default()
            },
        )
        .await?;

    framework
        .wait_for_rollout(
            &namespace,
            &format!("daemonset/{}", agent_override_name),
            vec!["--timeout=6000s"],
        )
        .await?;

    framework
        .wait_for_rollout(
            &namespace,
            &format!("statefulset/{}", aggregator_override_name),
            vec!["--timeout=60s"],
        )
        .await?;

    let test_namespace = framework.namespace(&pod_namespace).await?;

    let test_pod = framework
        .test_pod(test_pod::Config::from_pod(&make_test_pod(
            &pod_namespace,
            "test-pod",
            "echo MARKER",
            vec![],
            vec![],
        ))?)
        .await?;

    framework
        .wait(
            &pod_namespace,
            vec!["pods/test-pod"],
            WaitFor::Condition("initialized"),
            vec!["--timeout=60s"],
        )
        .await?;

    let mut log_reader = framework.logs(
        &namespace,
        &format!("statefulset/{}", aggregator_override_name),
    )?;
    smoke_check_first_line(&mut log_reader).await;

    // Read the rest of the log lines.
    let mut got_marker = false;
    look_for_log_line(&mut log_reader, |val| {
        if val["kubernetes"]["pod_namespace"] != pod_namespace {
            // A log from something other than our test pod, pretend we don't
            // see it.
            return FlowControlCommand::GoOn;
        }

        // Ensure we got the marker.
        assert_eq!(val["message"], "MARKER");

        if got_marker {
            // We've already seen one marker! This is not good, we only emitted
            // one.
            panic!("Marker seen more than once");
        }

        // If we did, remember it.
        got_marker = true;

        // Request to stop the flow.
        FlowControlCommand::Terminate
    })
    .await?;

    assert!(got_marker);

    drop(test_pod);
    drop(test_namespace);
    drop(vector);
    Ok(())
}
