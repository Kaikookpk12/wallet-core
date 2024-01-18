use crate::{aliases::*, BitcoinEntry};
use crate::{Error, Result};
use tw_coin_entry::coin_entry::CoinEntry;
use tw_coin_entry::modules::plan_builder::PlanBuilder;
use tw_coin_entry::signing_output_error;
use tw_proto::BitcoinV2::Proto;
use tw_proto::BitcoinV2::Proto::mod_Input::InputBrc20Inscription;
use tw_proto::Utxo::Proto as UtxoProto;

pub struct BitcoinPlanBuilder;

impl PlanBuilder for BitcoinPlanBuilder {
    type SigningInput<'a> = Proto::ComposePlan<'a>;
    type Plan = Proto::TransactionPlan<'static>;

    #[inline]
    fn plan(
        &self,
        _coin: &dyn tw_coin_entry::coin_context::CoinContext,
        proto: Self::SigningInput<'_>,
    ) -> Self::Plan {
        self.plan_impl(_coin, proto)
            .unwrap_or_else(|err| signing_output_error!(Proto::TransactionPlan, err))
    }
}

impl BitcoinPlanBuilder {
    fn plan_impl(
        &self,
        _coin: &dyn tw_coin_entry::coin_context::CoinContext,
        proto: Proto::ComposePlan<'_>,
    ) -> Result<Proto::TransactionPlan<'static>> {
        let plan = match proto.compose {
            Proto::mod_ComposePlan::OneOfcompose::brc20(plan) => {
                let built_plan = self.plan_brc20(_coin, plan)?;

                Proto::TransactionPlan {
                    error: Proto::Error::OK,
                    error_message: Default::default(),
                    plan: Proto::mod_TransactionPlan::OneOfplan::brc20(built_plan),
                }
            },
            _ => panic!(),
        };

        Ok(plan)
    }
    fn plan_brc20(
        &self,
        _coin: &dyn tw_coin_entry::coin_context::CoinContext,
        proto: Proto::mod_ComposePlan::ComposeBrc20Plan<'_>,
    ) -> Result<Proto::mod_TransactionPlan::Brc20Plan<'static>> {
        // Hard-clones
        let inscription = proto
            .inscription
            .ok_or_else(|| Error::from(Proto::Error::Error_missing_inscription))?;

        let brc20_info = InputBrc20Inscription {
            one_prevout: inscription.one_prevout,
            inscribe_to: inscription.inscribe_to.to_vec().into(),
            ticker: inscription.ticker.to_string().into(),
            transfer_amount: inscription.transfer_amount,
        };

        let tagged_output = super::utils::hard_clone_proto_output(
            proto
                .tagged_output
                .ok_or_else(|| Error::from(Proto::Error::Error_missing_tagged_output))?,
        )?;

        // First, we create the reveal transaction in order to calculate its
        // input requirement (fee + dust limit).

        // We can use a zeroed Txid here.
        let txid = vec![0; 32];
        let dummy_brc20_input = Proto::Input {
            txid: txid.into(),
            // The value is not relevant here, but we raise it above the output
            // or we get an error.
            value: u64::MAX,
            sighash_type: UtxoProto::SighashType::UseDefault,
            to_recipient: ProtoInputRecipient::builder(Proto::mod_Input::InputBuilder {
                variant: ProtoInputBuilder::brc20_inscribe(brc20_info.clone()),
            }),
            ..Default::default()
        };

        let dummy_reveal = Proto::SigningInput {
            inputs: vec![dummy_brc20_input],
            outputs: vec![tagged_output.clone()],
            // The reveal transaction has exactly one input.
            input_selector: UtxoProto::InputSelector::UseAll,
            // Disable change output creation.
            fee_per_vb: proto.fee_per_vb,
            disable_change_output: true,
            ..Default::default()
        };

        // We can now determine the fee of the reveal transaction.
        let dummy_signed = BitcoinEntry.preimage_hashes(_coin, dummy_reveal);
        if dummy_signed.error != Proto::Error::OK {
            return Err(Error::from(dummy_signed.error));
        }

        // The estimated fee for the REVEAL transaction.
        let reveal_fee_estimate = dummy_signed.fee_estimate;

        // Create the BRC20 output for the COMMIT transaction; we set the
        // amount to the estimated fee (REVEAL) plus the dust limit (`tagged_output.value`).
        let brc20_output = Proto::Output {
            // Consider the reveal fee plus the tagged output value (normally 546 sats).
            value: reveal_fee_estimate + tagged_output.value,
            to_recipient: ProtoOutputRecipient::builder(Proto::mod_Output::OutputBuilder {
                variant: ProtoOutputBuilder::brc20_inscribe(
                    Proto::mod_Output::OutputBrc20Inscription {
                        inscribe_to: brc20_info.inscribe_to.to_vec().into(),
                        ticker: brc20_info.ticker.to_string().into(),
                        transfer_amount: brc20_info.transfer_amount,
                    },
                ),
            }),
        };

        let brc20_output_value = brc20_output.value;

        // Clone the change output, if provided.
        let change_output = if let Some(change) = proto.change_output {
            Some(super::utils::hard_clone_proto_output(change)?)
        } else {
            None
        };

        // Create the actual COMMIT transaction with the appropriately selected inputs.
        let commit_signing = Proto::SigningInput {
            private_key: proto.private_key.to_vec().into(),
            // We pass on the full specified input set. The library will select
            // this accordingly depending on the input selector.
            inputs: proto
                .inputs
                .iter()
                .cloned()
                .map(super::utils::hard_clone_proto_input)
                .collect::<Result<_>>()?,
            outputs: vec![brc20_output],
            input_selector: proto.input_selector,
            change_output: change_output.clone(),
            fee_per_vb: proto.fee_per_vb,
            disable_change_output: proto.disable_change_output,
            ..Default::default()
        };

        // We now determine the Txid of the COMMIT transaction, which we will have
        // to use in the REVEAL transaction.
        let commit_signed = BitcoinEntry.sign(_coin, commit_signing.clone());
        if commit_signed.error != Proto::Error::OK {
            return Err(Error::from(commit_signed.error));
        }

        let commit_txid: Vec<u8> = commit_signed.txid.to_vec().iter().copied().rev().collect();

        // Create the actual REVEAL transaction with the generated COMMIT input.
        let brc20_input = Proto::Input {
            value: brc20_output_value,
            // Reference COMMIT transaction.
            txid: commit_txid.into(),
            sighash_type: UtxoProto::SighashType::UseDefault,
            to_recipient: ProtoInputRecipient::builder(Proto::mod_Input::InputBuilder {
                variant: ProtoInputBuilder::brc20_inscribe(brc20_info.clone()),
            }),
            ..Default::default()
        };

        // Build the REVEAL transaction.
        let reveal_signing = Proto::SigningInput {
            private_key: proto.private_key.to_vec().into(),
            inputs: vec![brc20_input],
            outputs: vec![tagged_output],
            // Only one input, the COMMIT transaction.
            input_selector: UtxoProto::InputSelector::UseAll,
            fee_per_vb: proto.fee_per_vb,
            // No change output.
            disable_change_output: true,
            ..Default::default()
        };

        let reveal_signed = BitcoinEntry.sign(_coin, reveal_signing);

        Ok(Proto::mod_TransactionPlan::Brc20Plan {
            commit: Some(commit_signed),
            reveal: Some(reveal_signed),
        })
    }
}
