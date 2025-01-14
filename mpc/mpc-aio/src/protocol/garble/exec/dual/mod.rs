//! An implementation of "Dual Execution" mode which provides authenticity but allows a malicious
//! party to learn n bits of the other party's input with 1/2^n probability of it going undetected.
//!
//! Important! Because currently we do not implement a maliciously secure equality check,
//! all private inputs of the [`DualExFollower`] may be leaked if the [`DualExLeader`] is
//! malicious. Such leakage, however, will be detected by the [`DualExFollower`] during the
//! equality check.

mod follower;
mod leader;
pub mod state;

pub use follower::DualExFollower;
pub use leader::DualExLeader;

use async_trait::async_trait;

use crate::protocol::{
    garble::{GCError, GarbleChannel, GarbleMessage},
    ot::{OTFactoryError, ObliviousReceive, ObliviousSend},
};
use futures::{SinkExt, StreamExt};
use mpc_circuits::{Input, InputValue, OutputValue, WireGroup};
use mpc_core::{
    garble::{
        exec::dual::{DESummary, DualExConfig},
        ActiveEncodedInput, ActiveInputSet, FullEncodedInput, FullInputSet,
    },
    ot::config::{
        OTReceiverConfig, OTReceiverConfigBuilder, OTSenderConfig, OTSenderConfigBuilder,
    },
};
use utils_aio::{expect_msg_or_err, factory::AsyncFactory};

#[async_trait]
pub trait DEExecute {
    /// Execute dual execution protocol
    ///
    /// Returns decoded output values
    async fn execute(
        self,
        gen_labels: FullInputSet,
        gen_inputs: Vec<InputValue>,
        ot_send_inputs: Vec<Input>,
        ot_receive_inputs: Vec<InputValue>,
        cached_labels: Vec<ActiveEncodedInput>,
    ) -> Result<Vec<OutputValue>, GCError>;

    /// Execute dual execution protocol
    ///
    /// Returns the output values and a summary of the execution.
    async fn execute_and_summarize(
        mut self,
        gen_labels: FullInputSet,
        gen_inputs: Vec<InputValue>,
        ot_send_inputs: Vec<Input>,
        ot_receive_inputs: Vec<InputValue>,
        cached_labels: Vec<ActiveEncodedInput>,
    ) -> Result<(Vec<OutputValue>, DESummary), GCError>;

    /// Execute dual execution protocol without the equality check
    ///
    /// This can be used when chaining multiple circuits together. Neither party
    /// reveals the output label decoding information.
    ///
    /// ** Warning **
    ///
    /// Do not use this method unless you know what you're doing! The output labels returned
    /// by this method can _not_ be considered correct without the equality check.
    ///
    /// Returns a summary of the execution.
    async fn execute_skip_equality_check(
        mut self,
        gen_labels: FullInputSet,
        gen_inputs: Vec<InputValue>,
        ot_send_inputs: Vec<Input>,
        ot_receive_inputs: Vec<InputValue>,
        cached_labels: Vec<ActiveEncodedInput>,
    ) -> Result<DESummary, GCError>;
}

/// Set up input labels by exchanging directly and via oblivious transfer.
pub async fn setup_inputs_with<LSF, LRF, LS, LR>(
    label_sender_id: String,
    label_receiver_id: String,
    channel: &mut GarbleChannel,
    label_sender_factory: &mut LSF,
    label_receiver_factory: &mut LRF,
    gen_labels: FullInputSet,
    gen_inputs: Vec<InputValue>,
    ot_send_inputs: Vec<Input>,
    ot_receive_inputs: Vec<InputValue>,
    cached_labels: Vec<ActiveEncodedInput>,
) -> Result<((FullInputSet, ActiveInputSet), (Option<LS>, Option<LR>)), GCError>
where
    LSF: AsyncFactory<LS, Config = OTSenderConfig, Error = OTFactoryError> + Send,
    LRF: AsyncFactory<LR, Config = OTReceiverConfig, Error = OTFactoryError> + Send,
    LS: ObliviousSend<FullEncodedInput> + Send,
    LR: ObliviousReceive<InputValue, ActiveEncodedInput> + Send,
{
    let circ = gen_labels.circuit();

    // Collect labels to be sent via OT
    let ot_send_labels = ot_send_inputs
        .iter()
        .map(|input| gen_labels[input.index()].clone())
        .collect::<Vec<FullEncodedInput>>();

    // Collect active labels to be directly sent
    let direct_send_labels = gen_inputs
        .iter()
        .map(|input| {
            gen_labels[input.index()]
                .select(input.value())
                .expect("Input value should be valid")
        })
        .collect::<Vec<ActiveEncodedInput>>();

    // Concurrently execute oblivious transfers and direct label sending

    // If there are no labels to be sent via OT, we can skip the OT protocol
    let ot_send_fut = async move {
        if ot_send_labels.len() > 0 {
            let count = ot_send_labels.iter().map(|labels| labels.len()).sum();

            let sender_config = OTSenderConfigBuilder::default()
                .count(count)
                .build()
                .expect("OTSenderConfig should be valid");

            let mut label_sender = label_sender_factory
                .create(label_sender_id, sender_config)
                .await?;

            let _ = label_sender.send(ot_send_labels).await?;

            Result::<_, GCError>::Ok(Some(label_sender))
        } else {
            Result::<_, GCError>::Ok(None)
        }
    };

    let direct_send_fut = channel.send(GarbleMessage::InputLabels(
        direct_send_labels
            .into_iter()
            .map(|labels| labels.into())
            .collect::<Vec<_>>(),
    ));

    // If there are no labels to be received via OT, we can skip the OT protocol
    let ot_receive_fut = async move {
        if ot_receive_inputs.len() > 0 {
            let count = ot_receive_inputs.iter().map(|input| input.len()).sum();

            let receiver_config = OTReceiverConfigBuilder::default()
                .count(count)
                .build()
                .expect("OTReceiverConfig should be valid");

            let mut label_receiver = label_receiver_factory
                .create(label_receiver_id, receiver_config)
                .await?;

            let ot_receive_labels = label_receiver.receive(ot_receive_inputs).await?;

            Result::<_, GCError>::Ok((ot_receive_labels, Some(label_receiver)))
        } else {
            Result::<_, GCError>::Ok((vec![], None))
        }
    };

    let (ot_send_result, direct_send_result, ot_receive_result) =
        futures::join!(ot_send_fut, direct_send_fut, ot_receive_fut);

    let label_sender = ot_send_result?;
    direct_send_result?;
    let (ot_receive_labels, label_receiver) = ot_receive_result?;

    // Expect direct labels from peer
    let msg = expect_msg_or_err!(
        channel.next().await,
        GarbleMessage::InputLabels,
        GCError::Unexpected
    )?;

    let direct_received_labels = msg
        .into_iter()
        .map(|msg| ActiveEncodedInput::from_unchecked(&circ, msg.into()))
        .collect::<Result<Vec<_>, _>>()?;

    // Collect all active labels into a set
    let ev_labels =
        ActiveInputSet::new([ot_receive_labels, direct_received_labels, cached_labels].concat())?;

    Ok(((gen_labels, ev_labels), (label_sender, label_receiver)))
}

#[cfg(feature = "mock")]
pub mod mock {
    use super::{state::Initialized, *};
    use crate::protocol::{
        garble::backend::RayonBackend,
        ot::mock::{MockOTFactory, MockOTReceiver, MockOTSender},
    };
    use mpc_core::Block;
    use utils_aio::duplex::DuplexChannel;

    pub type MockDualExLeader = DualExLeader<
        Initialized,
        RayonBackend,
        MockOTFactory<Block>,
        MockOTFactory<Block>,
        MockOTSender<Block>,
        MockOTReceiver<Block>,
    >;

    pub type MockDualExFollower = DualExFollower<
        Initialized,
        RayonBackend,
        MockOTFactory<Block>,
        MockOTFactory<Block>,
        MockOTSender<Block>,
        MockOTReceiver<Block>,
    >;

    pub fn mock_dualex_pair(config: DualExConfig) -> (MockDualExLeader, MockDualExFollower) {
        let (leader_channel, follower_channel) = DuplexChannel::<GarbleMessage>::new();
        let ot_factory = MockOTFactory::new();

        let leader = DualExLeader::new(
            config.clone(),
            Box::new(leader_channel),
            RayonBackend,
            ot_factory.clone(),
            ot_factory.clone(),
        );

        let follower = DualExFollower::new(
            config,
            Box::new(follower_channel),
            RayonBackend,
            ot_factory.clone(),
            ot_factory.clone(),
        );

        (leader, follower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mock::*;
    use mpc_circuits::{Circuit, ADDER_64};
    use mpc_core::garble::exec::dual::DualExConfigBuilder;
    use rand::SeedableRng;
    use rand_chacha::ChaCha12Rng;

    #[tokio::test]
    async fn test_dualex() {
        let mut rng = ChaCha12Rng::seed_from_u64(0);
        let circ = Circuit::load_bytes(ADDER_64).unwrap();
        let config = DualExConfigBuilder::default()
            .id("test".to_string())
            .circ(circ.clone())
            .build()
            .unwrap();
        let (leader, follower) = mock_dualex_pair(config);

        let leader_input = circ.input(0).unwrap().to_value(1u64).unwrap();
        let follower_input = circ.input(1).unwrap().to_value(2u64).unwrap();

        let leader_labels = FullInputSet::generate(&mut rng, &circ, None);
        let follower_labels = FullInputSet::generate(&mut rng, &circ, None);

        let leader_task = {
            let leader_input = leader_input.clone();
            let follower_input = follower_input.clone();
            tokio::spawn(async move {
                leader
                    .setup_inputs(
                        leader_labels,
                        vec![leader_input.clone()],
                        vec![follower_input.group().clone()],
                        vec![leader_input.clone()],
                        vec![],
                    )
                    .await
                    .unwrap()
                    .execute()
                    .await
                    .unwrap()
            })
        };

        let follower_task = tokio::spawn(async move {
            follower
                .setup_inputs(
                    follower_labels,
                    vec![follower_input.clone()],
                    vec![leader_input.group().clone()],
                    vec![follower_input],
                    vec![],
                )
                .await
                .unwrap()
                .execute()
                .await
                .unwrap()
        });

        let (leader_out, follower_out) = tokio::join!(leader_task, follower_task);

        let expected_out = circ.output(0).unwrap().to_value(3u64).unwrap();

        let leader_out = leader_out.unwrap();
        let follower_out = follower_out.unwrap();

        assert_eq!(expected_out, leader_out[0]);
        assert_eq!(leader_out, follower_out);
    }
}
