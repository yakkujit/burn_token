//! The request router module decides which randomness backend is used

use cosmwasm_std::{
    to_binary, Binary, CosmosMsg, DepsMut, Env, HexBinary, IbcMsg, StdError, StdResult, Timestamp,
};
use drand_common::{round_after, DRAND_CHAIN_HASH};
use nois_protocol::{
    DeliverBeaconPacket, RequestBeaconPacketAck, StdAck, DELIVER_BEACON_PACKET_LIFETIME,
};

use crate::{
    drand_archive::{archive_lookup, archive_store},
    state::{
        increment_processed_drand_jobs, unprocessed_drand_jobs_dequeue,
        unprocessed_drand_jobs_enqueue, unprocessed_drand_jobs_len, Job,
    },
};

/// The number of jobs that are processed per submission. Use this limit
/// to ensure the gas usage for the submissions is relatively stable.
///
/// Currently a submission without jobs consumes ~600k gas. Every job adds
/// ~50k gas.
const MAX_JOBS_PER_SUBMISSION_WITH_VERIFICATION: u32 = 2;
const MAX_JOBS_PER_SUBMISSION_WITHOUT_VERIFICATION: u32 = 14;

pub struct RoutingReceipt {
    pub acknowledgement: StdAck,
    pub msgs: Vec<CosmosMsg>,
}

pub struct NewDrand {
    pub msgs: Vec<CosmosMsg>,
    pub jobs_processed: u32,
    pub jobs_left: u32,
}

pub struct RequestRouter {}

impl RequestRouter {
    pub fn new() -> Self {
        Self {}
    }

    pub fn route(
        &self,
        deps: DepsMut,
        env: Env,
        channel: String,
        after: Timestamp,
        origin: Binary,
    ) -> StdResult<RoutingReceipt> {
        // Here we currently only have one backend
        self.handle_drand(deps, env, channel, after, origin)
    }

    fn handle_drand(
        &self,
        deps: DepsMut,
        env: Env,
        channel: String,
        after: Timestamp,
        origin: Binary,
    ) -> StdResult<RoutingReceipt> {
        let (round, source_id) = commit_to_drand_round(after);

        let existing_randomness = archive_lookup(deps.storage, round);

        let job = Job {
            source_id: source_id.clone(),
            channel,
            origin,
        };

        let mut msgs = Vec::<CosmosMsg>::new();

        let acknowledgement = if let Some(randomness) = existing_randomness {
            //If the drand round already exists we send it
            increment_processed_drand_jobs(deps.storage, round)?;
            let msg = create_deliver_beacon_ibc_message(env.block.time, job, randomness)?;
            msgs.push(msg.into());
            StdAck::success(&RequestBeaconPacketAck::Processed { source_id })
        } else {
            unprocessed_drand_jobs_enqueue(deps.storage, round, &job)?;
            StdAck::success(&RequestBeaconPacketAck::Queued { source_id })
        };

        Ok(RoutingReceipt {
            acknowledgement,
            msgs,
        })
    }

    pub fn new_drand(
        &self,
        deps: DepsMut,
        env: Env,
        round: u64,
        randomness: &HexBinary,
        is_verifying_tx: bool,
    ) -> StdResult<NewDrand> {
        archive_store(deps.storage, round, randomness);

        let max_jobs_per_submission = if is_verifying_tx {
            MAX_JOBS_PER_SUBMISSION_WITH_VERIFICATION
        } else {
            MAX_JOBS_PER_SUBMISSION_WITHOUT_VERIFICATION
        };

        let mut msgs = Vec::<CosmosMsg>::new();
        let mut jobs_processed = 0;
        // let max_jobs_per_submission
        while let Some(job) = unprocessed_drand_jobs_dequeue(deps.storage, round)? {
            increment_processed_drand_jobs(deps.storage, round)?;
            // Use IbcMsg::SendPacket to send packages to the proxies.
            let msg = create_deliver_beacon_ibc_message(env.block.time, job, randomness.clone())?;
            msgs.push(msg.into());
            jobs_processed += 1;
            if jobs_processed >= max_jobs_per_submission {
                break;
            }
        }
        let jobs_left = unprocessed_drand_jobs_len(deps.storage, round)?;
        Ok(NewDrand {
            msgs,
            jobs_processed,
            jobs_left,
        })
    }
}

/// Takes the job and turns it into a an IBC message with a `DeliverBeaconPacket`.
fn create_deliver_beacon_ibc_message(
    blocktime: Timestamp,
    job: Job,
    randomness: HexBinary,
) -> Result<IbcMsg, StdError> {
    let packet = DeliverBeaconPacket {
        randomness,
        source_id: job.source_id,
        origin: job.origin,
    };
    let msg = IbcMsg::SendPacket {
        channel_id: job.channel,
        data: to_binary(&packet)?,
        timeout: blocktime
            .plus_seconds(DELIVER_BEACON_PACKET_LIFETIME)
            .into(),
    };
    Ok(msg)
}

/// Calculates the next round in the future, i.e. publish time > base time.
fn commit_to_drand_round(after: Timestamp) -> (u64, String) {
    let round = round_after(after);
    let source_id = format!("drand:{}:{}", DRAND_CHAIN_HASH, round);
    (round, source_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_to_drand_round_works() {
        // UNIX epoch
        let (round, source) = commit_to_drand_round(Timestamp::from_seconds(0));
        assert_eq!(round, 1);
        assert_eq!(
            source,
            "drand:8990e7a9aaed2ffed73dbd7092123d6f289930540d7651336225dc172e51b2ce:1"
        );

        // Before Drand genesis (https://api3.drand.sh/info)
        let (round, source) =
            commit_to_drand_round(Timestamp::from_seconds(1595431050).minus_nanos(1));
        assert_eq!(round, 1);
        assert_eq!(
            source,
            "drand:8990e7a9aaed2ffed73dbd7092123d6f289930540d7651336225dc172e51b2ce:1"
        );

        // At Drand genesis (https://api3.drand.sh/info)
        let (round, source) = commit_to_drand_round(Timestamp::from_seconds(1595431050));
        assert_eq!(round, 2);
        assert_eq!(
            source,
            "drand:8990e7a9aaed2ffed73dbd7092123d6f289930540d7651336225dc172e51b2ce:2"
        );

        // After Drand genesis (https://api3.drand.sh/info)
        let (round, _) = commit_to_drand_round(Timestamp::from_seconds(1595431050).plus_nanos(1));
        assert_eq!(round, 2);

        // Drand genesis +29s/30s/31s
        let (round, _) =
            commit_to_drand_round(Timestamp::from_seconds(1595431050).plus_seconds(29));
        assert_eq!(round, 2);
        let (round, _) =
            commit_to_drand_round(Timestamp::from_seconds(1595431050).plus_seconds(30));
        assert_eq!(round, 3);
        let (round, _) =
            commit_to_drand_round(Timestamp::from_seconds(1595431050).plus_seconds(31));
        assert_eq!(round, 3);
    }
}
