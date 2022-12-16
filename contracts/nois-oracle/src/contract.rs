use cosmwasm_std::{
    entry_point, from_binary, from_slice, to_binary, Addr, Attribute, BankMsg, Coin, CosmosMsg,
    Deps, DepsMut, Empty, Env, Event, HexBinary, Ibc3ChannelOpenResponse, IbcBasicResponse,
    IbcChannelCloseMsg, IbcChannelConnectMsg, IbcChannelOpenMsg, IbcChannelOpenResponse, IbcMsg,
    IbcPacketAckMsg, IbcPacketReceiveMsg, IbcPacketTimeoutMsg, IbcReceiveResponse, MessageInfo,
    Order, QueryResponse, Response, StdError, StdResult, Timestamp,
};
use cw_storage_plus::Bound;
use drand_verify::{derive_randomness, g1_from_fixed_unchecked, verify};
use nois_protocol::{
    check_order, check_version, DeliverBeaconPacket, DeliverBeaconPacketAck, Never,
    RequestBeaconPacket, RequestBeaconPacketAck, StdAck, DELIVER_BEACON_PACKET_LIFETIME,
    IBC_APP_VERSION,
};

use crate::bots::validate_moniker;
use crate::drand::{round_after, DRAND_CHAIN_HASH, DRAND_MAINNET_PUBKEY};
use crate::error::ContractError;
use crate::job_id::validate_job_id;
use crate::msg::{
    BeaconResponse, BeaconsResponse, BotResponse, BotsResponse, ConfigResponse, ExecuteMsg,
    InstantiateMsg, JobStatsResponse, QueriedSubmission, QueryMsg, SubmissionsResponse,
};
use crate::state::{
    get_processed_jobs, increment_processed_jobs, unprocessed_jobs_dequeue,
    unprocessed_jobs_enqueue, unprocessed_jobs_len, Bot, Config, Job, QueriedBeacon, QueriedBot,
    StoredSubmission, VerifiedBeacon, BEACONS, BOTS, CONFIG, SUBMISSIONS, SUBMISSIONS_ORDER,
    WHITELIST,
};

/// Constant defining how many submissions per round will be rewarded
const NUMBER_OF_INCENTIVES_PER_ROUND: u32 = 6;

/// The number of jobs that are processed per submission. Use this limit
/// to ensure the gas usage for the submissions is relatively stable.
///
/// Currently a submission without jobs consumes ~600k gas. Every job adds
/// ~50k gas.
const MAX_JOBS_PER_SUBMISSION: u32 = 3;

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    let config = Config {
        min_round: msg.min_round,
        incentive_amount: msg.incentive_amount,
        incentive_denom: msg.incentive_denom,
    };
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::default())
}

// This no-op migrate implementation allows us to upgrade within the 0.7 series.
// No state changes expected.
#[entry_point]
pub fn migrate(_deps: DepsMut, _env: Env, _msg: Empty) -> StdResult<Response> {
    Ok(Response::default())
}

#[entry_point]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::AddRound {
            round,
            previous_signature,
            signature,
        } => execute_add_round(deps, env, info, round, previous_signature, signature),
        ExecuteMsg::RegisterBot { moniker } => execute_register_bot(deps, env, info, moniker),
        ExecuteMsg::UpdateWhitelistBots {
            bots_to_whitelist,
            bots_to_dewhitelist,
        } => execute_update_whitelist_bots(deps, bots_to_whitelist, bots_to_dewhitelist),
    }
}

#[entry_point]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<QueryResponse> {
    let response = match msg {
        QueryMsg::Config {} => to_binary(&query_config(deps)?)?,
        QueryMsg::Beacon { round } => to_binary(&query_beacon(deps, round)?)?,
        QueryMsg::BeaconsAsc { start_after, limit } => {
            to_binary(&query_beacons(deps, start_after, limit, Order::Ascending)?)?
        }
        QueryMsg::BeaconsDesc { start_after, limit } => {
            to_binary(&query_beacons(deps, start_after, limit, Order::Descending)?)?
        }
        QueryMsg::Bot { address } => to_binary(&query_bot(deps, address)?)?,
        QueryMsg::Bots {} => to_binary(&query_bots(deps)?)?,
        QueryMsg::Submissions { round } => to_binary(&query_submissions(deps, round)?)?,
        QueryMsg::JobStats { round } => to_binary(&query_job_stats(deps, round)?)?,
    };
    Ok(response)
}

fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config = CONFIG.load(deps.storage)?;
    Ok(config)
}

// Query beacon by round
fn query_beacon(deps: Deps, round: u64) -> StdResult<BeaconResponse> {
    let beacon = BEACONS.may_load(deps.storage, round)?;
    Ok(BeaconResponse {
        beacon: beacon.map(|b| QueriedBeacon::make(b, round)),
    })
}

fn query_beacons(
    deps: Deps,
    start_after: Option<u64>,
    limit: Option<u32>,
    order: Order,
) -> StdResult<BeaconsResponse> {
    let limit: usize = limit.unwrap_or(100) as usize;
    let (low_bound, top_bound) = match order {
        Order::Ascending => (start_after.map(Bound::exclusive), None),
        Order::Descending => (None, start_after.map(Bound::exclusive)),
    };
    let beacons: Vec<QueriedBeacon> = BEACONS
        .range(deps.storage, low_bound, top_bound, order)
        .take(limit)
        .map(|c| c.map(|(round, beacon)| QueriedBeacon::make(beacon, round)))
        .collect::<Result<_, _>>()?;
    Ok(BeaconsResponse { beacons })
}

fn query_bot(deps: Deps, address: String) -> StdResult<BotResponse> {
    let address = deps.api.addr_validate(&address)?;
    let bot = BOTS
        .may_load(deps.storage, &address)?
        .map(|bot| QueriedBot::make(bot, address));
    Ok(BotResponse { bot })
}

fn query_bots(deps: Deps) -> StdResult<BotsResponse> {
    // No pagination here yet 🤷‍♂️
    let bots = BOTS
        .range(deps.storage, None, None, Order::Ascending)
        .map(|result| {
            let (address, bot) = result.unwrap();
            QueriedBot::make(bot, address)
        })
        .collect();
    Ok(BotsResponse { bots })
}

// Query submissions by round
fn query_submissions(deps: Deps, round: u64) -> StdResult<SubmissionsResponse> {
    let prefix = SUBMISSIONS_ORDER.prefix(round);

    let submission_addresses: Vec<Addr> = prefix
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| -> StdResult<_> { Ok(item?.1) })
        .collect::<Result<_, _>>()?;
    let mut submissions: Vec<QueriedSubmission> = Vec::with_capacity(submission_addresses.len());
    for addr in submission_addresses {
        let stored = SUBMISSIONS.load(deps.storage, (round, &addr))?;
        submissions.push(QueriedSubmission::make(stored, addr));
    }
    Ok(SubmissionsResponse { round, submissions })
}

// Query job stats by round
fn query_job_stats(deps: Deps, round: u64) -> StdResult<JobStatsResponse> {
    let unprocessed = unprocessed_jobs_len(deps.storage, round)?;
    let processed = get_processed_jobs(deps.storage, round)?;
    Ok(JobStatsResponse {
        round,
        unprocessed,
        processed,
    })
}

#[entry_point]
/// enforces ordering and versioing constraints
pub fn ibc_channel_open(
    _deps: DepsMut,
    _env: Env,
    msg: IbcChannelOpenMsg,
) -> Result<IbcChannelOpenResponse, ContractError> {
    let channel = msg.channel();

    check_order(&channel.order)?;
    // In ibcv3 we don't check the version string passed in the message
    // and only check the counterparty version.
    if let Some(counter_version) = msg.counterparty_version() {
        check_version(counter_version)?;
    }

    // We return the version we need (which could be different than the counterparty version)
    Ok(Some(Ibc3ChannelOpenResponse {
        version: IBC_APP_VERSION.to_string(),
    }))
}

#[entry_point]
pub fn ibc_channel_connect(
    _deps: DepsMut,
    _env: Env,
    msg: IbcChannelConnectMsg,
) -> StdResult<IbcBasicResponse> {
    let channel = msg.channel();
    let chan_id = &channel.endpoint.channel_id;

    Ok(IbcBasicResponse::new()
        .add_attribute("action", "ibc_connect")
        .add_attribute("channel_id", chan_id)
        .add_event(Event::new("ibc").add_attribute("channel", "connect")))
}

#[entry_point]
pub fn ibc_channel_close(
    _deps: DepsMut,
    _env: Env,
    msg: IbcChannelCloseMsg,
) -> StdResult<IbcBasicResponse> {
    let channel = msg.channel();
    // get contract address and remove lookup
    let channel_id = channel.endpoint.channel_id.as_str();

    Ok(IbcBasicResponse::new()
        .add_attribute("action", "ibc_close")
        .add_attribute("channel_id", channel_id))
}

#[entry_point]
pub fn ibc_packet_receive(
    deps: DepsMut,
    env: Env,
    msg: IbcPacketReceiveMsg,
) -> Result<IbcReceiveResponse, Never> {
    let packet = msg.packet;
    // which local channel did this packet come on
    let channel = packet.dest.channel_id;

    // put this in a closure so we can convert all error responses into acknowledgements
    (|| {
        let msg: RequestBeaconPacket = from_slice(&packet.data)?;
        receive_request_beacon(deps, env, channel, msg.after, msg.sender, msg.job_id)
    })()
    .or_else(|e| {
        // we try to capture all app-level errors and convert them into
        // acknowledgement packets that contain an error code.
        let acknowledgement = StdAck::error(format!("Error processing packet: {e}"));
        Ok(IbcReceiveResponse::new()
            .set_ack(acknowledgement)
            .add_event(Event::new("ibc").add_attribute("packet", "receive")))
    })
}

fn receive_request_beacon(
    deps: DepsMut,
    env: Env,
    channel: String,
    after: Timestamp,
    sender: String,
    job_id: String,
) -> Result<IbcReceiveResponse, ContractError> {
    validate_job_id(&job_id)?;

    let (round, source_id) = commit_to_drand_round(after);

    let job = Job {
        source_id: source_id.clone(),
        channel,
        sender,
        job_id,
    };

    let beacon = BEACONS.may_load(deps.storage, round)?;

    let mut msgs = Vec::<CosmosMsg>::new();

    let acknowledgement = if let Some(beacon) = beacon.as_ref() {
        //If the drand round already exists we send it
        increment_processed_jobs(deps.storage, round)?;
        let msg = create_deliver_beacon_ibc_message(env.block.time, job, beacon)?;
        msgs.push(msg.into());
        StdAck::success(&RequestBeaconPacketAck::Processed { source_id })
    } else {
        unprocessed_jobs_enqueue(deps.storage, round, &job)?;
        StdAck::success(&RequestBeaconPacketAck::Queued { source_id })
    };

    Ok(IbcReceiveResponse::new()
        .set_ack(acknowledgement)
        .add_messages(msgs)
        .add_attribute("action", "receive_request_beacon"))
}

/// Takes the job and turns it into a an IBC message with a `DeliverBeaconPacket`.
fn create_deliver_beacon_ibc_message(
    blocktime: Timestamp,
    job: Job,
    beacon: &VerifiedBeacon,
) -> Result<IbcMsg, ContractError> {
    let packet = DeliverBeaconPacket {
        sender: job.sender,
        job_id: job.job_id,
        randomness: beacon.randomness.clone(),
        source_id: job.source_id,
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

#[entry_point]
pub fn ibc_packet_ack(
    _deps: DepsMut,
    _env: Env,
    msg: IbcPacketAckMsg,
) -> Result<IbcBasicResponse, ContractError> {
    let ack: StdAck = from_binary(&msg.acknowledgement.data)?;
    match ack {
        StdAck::Result(data) => {
            let _response: DeliverBeaconPacketAck = from_binary(&data)?;
            // alright
            Ok(IbcBasicResponse::new().add_attribute("action", "ibc_packet_ack"))
        }
        StdAck::Error(err) => Err(ContractError::ForeignError { err }),
    }
}

#[entry_point]
/// never should be called as we do not send packets
pub fn ibc_packet_timeout(
    _deps: DepsMut,
    _env: Env,
    _msg: IbcPacketTimeoutMsg,
) -> StdResult<IbcBasicResponse> {
    Ok(IbcBasicResponse::new().add_attribute("action", "ibc_packet_timeout"))
}

fn execute_register_bot(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    moniker: String,
) -> Result<Response, ContractError> {
    validate_moniker(&moniker)?;
    let bot = match BOTS.may_load(deps.storage, &info.sender)? {
        Some(mut bot) => {
            bot.moniker = moniker;
            bot
        }
        _ => Bot {
            moniker,
            rounds_added: 0,
        },
    };
    BOTS.save(deps.storage, &info.sender, &bot)?;
    Ok(Response::default())
}

fn execute_update_whitelist_bots(
    deps: DepsMut,
    bots_to_whitelist: Vec<String>,
    bots_to_dewhitelist: Vec<String>,
) -> Result<Response, ContractError> {
    bots_to_dewhitelist.into_iter().for_each(|bot| {
        let addr = deps.api.addr_validate(&bot).unwrap();
        if WHITELIST.has(deps.storage, &addr) {
            WHITELIST.remove(deps.storage, &addr);
        }
    });

    bots_to_whitelist.into_iter().for_each(|bot| {
        let addr = deps.api.addr_validate(&bot).unwrap();
        if !WHITELIST.has(deps.storage, &addr) {
            WHITELIST.save(deps.storage, &addr, &()).unwrap();
        }
    });

    Ok(Response::default())
}

fn execute_add_round(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    round: u64,
    previous_signature: HexBinary,
    signature: HexBinary,
) -> Result<Response, ContractError> {
    // Handle sender is not sending funds
    if !info.funds.is_empty() {
        return Err(StdError::generic_err("Do not send funds").into());
    }

    let config = CONFIG.load(deps.storage)?;
    let min_round = config.min_round;
    if round < min_round {
        return Err(ContractError::RoundTooLow { round, min_round });
    }

    let pk = g1_from_fixed_unchecked(DRAND_MAINNET_PUBKEY)
        .map_err(|_| ContractError::InvalidPubkey {})?;
    if !verify(&pk, round, &previous_signature, &signature).unwrap_or(false) {
        return Err(ContractError::InvalidSignature {});
    }

    let randomness: HexBinary = derive_randomness(signature.as_slice()).into();

    let beacon = &VerifiedBeacon {
        verified: env.block.time,
        randomness: randomness.clone(),
    };

    let submissions_key = (round, &info.sender);

    if SUBMISSIONS.has(deps.storage, submissions_key) {
        return Err(ContractError::SubmissionExists);
    }

    // True if and only if bot has been registered before
    let mut is_registered = false;

    if let Some(mut bot) = BOTS.may_load(deps.storage, &info.sender)? {
        is_registered = true;
        bot.rounds_added += 1;
        BOTS.save(deps.storage, &info.sender, &bot)?;
    }
    let mut is_whitelisted = false;
    if WHITELIST.has(deps.storage, &info.sender) {
        is_whitelisted = true;
    }

    SUBMISSIONS.save(
        deps.storage,
        submissions_key,
        &StoredSubmission {
            time: env.block.time,
        },
    )?;
    let prefix = SUBMISSIONS_ORDER.prefix(round);
    let next_index = match prefix
        .keys(deps.storage, None, None, Order::Descending)
        .next()
    {
        Some(x) => x? + 1, // The ? handles the decoding to u32
        None => 0,
    };
    SUBMISSIONS_ORDER.save(deps.storage, (round, next_index), &info.sender)?;

    let mut attributes = vec![
        Attribute::new("round", round.to_string()),
        Attribute::new("randomness", randomness.to_hex()),
        Attribute::new("worker", info.sender.to_string()),
    ];

    let mut out_msgs = Vec::<CosmosMsg>::new();

    // Pay the bot incentive
    let is_eligible =
        is_registered && is_whitelisted && next_index < NUMBER_OF_INCENTIVES_PER_ROUND; // top X submissions can receive a reward
    if is_eligible {
        let contract_balance = deps
            .querier
            .query_balance(&env.contract.address, &config.incentive_denom)?
            .amount;
        let bot_desired_incentive = incentive_amount(&config);
        attributes.push(Attribute::new(
            "bot_incentive",
            bot_desired_incentive.to_string(),
        ));
        if contract_balance >= bot_desired_incentive.amount {
            out_msgs.push(
                BankMsg::Send {
                    to_address: info.sender.to_string(),
                    amount: vec![bot_desired_incentive],
                }
                .into(),
            );
        }
    }

    if !BEACONS.has(deps.storage, round) {
        // Round is new
        BEACONS.save(deps.storage, round, beacon)?;
    } else {
        // Round has already been verified and must not be overriden to not
        // get a wrong `verified` timestamp.
    }

    let mut jobs_processed = 0;
    while let Some(job) = unprocessed_jobs_dequeue(deps.storage, round)? {
        increment_processed_jobs(deps.storage, round)?;
        // Use IbcMsg::SendPacket to send packages to the proxies.
        let msg = create_deliver_beacon_ibc_message(env.block.time, job, beacon)?;
        out_msgs.push(msg.into());
        jobs_processed += 1;
        if jobs_processed >= MAX_JOBS_PER_SUBMISSION {
            break;
        }
    }

    Ok(Response::new()
        .add_messages(out_msgs)
        .add_attributes(attributes))
}

fn incentive_amount(config: &Config) -> Coin {
    Coin {
        denom: config.incentive_denom.clone(),
        amount: config.incentive_amount,
    }
}

#[cfg(test)]
mod tests {

    use crate::msg::ExecuteMsg;

    use super::*;
    use cosmwasm_std::testing::{
        mock_dependencies, mock_env, mock_ibc_channel_close_init, mock_ibc_channel_connect_ack,
        mock_ibc_channel_open_init, mock_ibc_channel_open_try, mock_ibc_packet_recv, mock_info,
        MockApi, MockQuerier, MockStorage,
    };
    use cosmwasm_std::{coin, from_binary, Addr, OwnedDeps, Uint128};
    use nois_protocol::{APP_ORDER, BAD_APP_ORDER};

    const CREATOR: &str = "creator";
    const TESTING_MIN_ROUND: u64 = 72785;

    fn setup() -> OwnedDeps<MockStorage, MockApi, MockQuerier> {
        let mut deps = mock_dependencies();
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        let info = mock_info(CREATOR, &[]);
        let res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(0, res.messages.len());
        deps
    }

    fn make_add_round_msg(round: u64) -> ExecuteMsg {
        match round {
            9 => ExecuteMsg::AddRound {
                // curl -sS https://drand.cloudflare.com/public/9
                round: 9,
                previous_signature: HexBinary::from_hex("b3ed3c540ef5c5407ea6dbf7407ca5899feeb54f66f7e700ee063db71f979a869d28efa9e10b5e6d3d24a838e8b6386a15b411946c12815d81f2c445ae4ee1a7732509f0842f327c4d20d82a1209f12dbdd56fd715cc4ed887b53c321b318cd7").unwrap(),
                signature: HexBinary::from_hex("99c37c83a0d7bb637f0e2f0c529aa5c8a37d0287535debe5dacd24e95b6e38f3394f7cb094bdf4908a192a3563276f951948f013414d927e0ba8c84466b4c9aea4de2a253dfec6eb5b323365dfd2d1cb98184f64c22c5293c8bfe7962d4eb0f5").unwrap(),
            },
            72785 => ExecuteMsg::AddRound {
                // curl -sS https://drand.cloudflare.com/public/72785
                round: 72785,
                previous_signature: HexBinary::from_hex("a609e19a03c2fcc559e8dae14900aaefe517cb55c840f6e69bc8e4f66c8d18e8a609685d9917efbfb0c37f058c2de88f13d297c7e19e0ab24813079efe57a182554ff054c7638153f9b26a60e7111f71a0ff63d9571704905d3ca6df0b031747").unwrap(),
                signature: HexBinary::from_hex("82f5d3d2de4db19d40a6980e8aa37842a0e55d1df06bd68bddc8d60002e8e959eb9cfa368b3c1b77d18f02a54fe047b80f0989315f83b12a74fd8679c4f12aae86eaf6ab5690b34f1fddd50ee3cc6f6cdf59e95526d5a5d82aaa84fa6f181e42").unwrap(),
            },
            72786 => ExecuteMsg::AddRound {
                // curl -sS https://drand.cloudflare.com/public/72786
                round: 72786,
                previous_signature: HexBinary::from_hex("82f5d3d2de4db19d40a6980e8aa37842a0e55d1df06bd68bddc8d60002e8e959eb9cfa368b3c1b77d18f02a54fe047b80f0989315f83b12a74fd8679c4f12aae86eaf6ab5690b34f1fddd50ee3cc6f6cdf59e95526d5a5d82aaa84fa6f181e42").unwrap(),
                signature: HexBinary::from_hex("85d64193239c6a2805b5953521c1e7c412d13f8b29df2dfc796b7dc8e1fd795b764362e49302956a350f9385f68b68d8085fda08c2bd0528984a413db52860b408c72d1210609de3a342259d4c08f86ee729a2dbeb140908270849fd7d0dec40").unwrap(),
            },
            72787 => ExecuteMsg::AddRound {
                // curl -sS https://drand.cloudflare.com/public/72787
                round: 72787,
                previous_signature: HexBinary::from_hex("85d64193239c6a2805b5953521c1e7c412d13f8b29df2dfc796b7dc8e1fd795b764362e49302956a350f9385f68b68d8085fda08c2bd0528984a413db52860b408c72d1210609de3a342259d4c08f86ee729a2dbeb140908270849fd7d0dec40").unwrap(),
                signature: HexBinary::from_hex("8ceee95d523f54a752807f4705ce0f89e69911dd3dce330a337b9409905a881a2f879d48fce499bfeeb3b12e7f83ab7d09b42f31fa729af4c19adfe150075b2f3fe99c8fbcd7b0b5f0bb91ac8ad8715bfe52e3fb12314fddb76d4e42461f6ea4").unwrap(),
            },
            2183668 => ExecuteMsg::AddRound {
                // curl -sS https://drand.cloudflare.com/public/2183668
                round: 2183668,
                previous_signature: HexBinary::from_hex("b0272269d87be8f146a0dc4f882b03add1e0f98ee7c55ee674107c231cfa7d2e40d9c88dd6e72f2f52d1abe14766b2c40dd392eec82d678a4c925c6937717246e8ae96d54d8ea70f85f8282cf14c56e5b547b7ee82df4ff61f3523a0eefcdf41").unwrap(),
                signature: HexBinary::from_hex("b06969214b8a7c8d705c4c5e00262626d95e30f8583dc21670508d6d4751ae95ddf675e76feabe1ee5f4000dd21f09d009bb2b57da6eedd10418e83c303c2d5845914175ffe13601574d039a7593c3521eaa98e43be927b4a00d423388501f05").unwrap(),
            },
            2183669 => ExecuteMsg::AddRound {
                // curl -sS https://drand.cloudflare.com/public/2183669
                round: 2183669,
                previous_signature: HexBinary::from_hex("b06969214b8a7c8d705c4c5e00262626d95e30f8583dc21670508d6d4751ae95ddf675e76feabe1ee5f4000dd21f09d009bb2b57da6eedd10418e83c303c2d5845914175ffe13601574d039a7593c3521eaa98e43be927b4a00d423388501f05").unwrap(),
                signature: HexBinary::from_hex("990538b0f0ca3b934f53eb41d7a4ba24f3b3800abfc06275eb843df75a53257c2dbfb8f6618bb72874a79303429db13e038e6619c08726e8bbb3ae58ebb31e08d2aed921e4246fdef984285eb679c6b443f24bd04f78659bd4230e654db4200d").unwrap(),
            },
            2183670 => ExecuteMsg::AddRound {
                // curl -sS https://drand.cloudflare.com/public/2183670
                round: 2183670,
                previous_signature: HexBinary::from_hex("990538b0f0ca3b934f53eb41d7a4ba24f3b3800abfc06275eb843df75a53257c2dbfb8f6618bb72874a79303429db13e038e6619c08726e8bbb3ae58ebb31e08d2aed921e4246fdef984285eb679c6b443f24bd04f78659bd4230e654db4200d").unwrap(),
                signature: HexBinary::from_hex("a63dcbd669534b049a86198ee98f1b68c24aac50de411d11f2a8a98414f9312cd04027810417d0fa60461c0533d604630ada568ef83af93ce05c1620c8bee1491092c11e5c7d9bb679b5b8de61bbb48e092164366ae6f799c082ddab691d1d78").unwrap(),
            },
            2183671 => ExecuteMsg::AddRound {
                // curl -sS https://drand.cloudflare.com/public/2183671
                round: 2183671,
                previous_signature: HexBinary::from_hex("a63dcbd669534b049a86198ee98f1b68c24aac50de411d11f2a8a98414f9312cd04027810417d0fa60461c0533d604630ada568ef83af93ce05c1620c8bee1491092c11e5c7d9bb679b5b8de61bbb48e092164366ae6f799c082ddab691d1d78").unwrap(),
                signature: HexBinary::from_hex("b449f94098616029baea233fa8b64851cf9de2b230a7c5a2181c3abdc9e92806ae9020a5d9dcdbb707b6f1754480954b00a80b594cb35b51944167d2b20cc3b3cac6da7023c6a6bf867c6c3844768794edcaae292394316603797d669f62691a").unwrap(),
            },
            _ => panic!("Test round {round} not set"),
        }
    }

    /// Adds round 72785, 72786, 72787
    fn add_test_rounds(mut deps: DepsMut, bot_addr: &str) {
        let msg = make_add_round_msg(72785);
        execute(deps.branch(), mock_env(), mock_info(bot_addr, &[]), msg).unwrap();
        let msg = make_add_round_msg(72786);
        execute(deps.branch(), mock_env(), mock_info(bot_addr, &[]), msg).unwrap();
        let msg = make_add_round_msg(72787);
        execute(deps.branch(), mock_env(), mock_info(bot_addr, &[]), msg).unwrap();
    }

    // connect will run through the entire handshake to set up a proper connect and
    // save the account (tested in detail in `proper_handshake_flow`)
    fn connect(mut deps: DepsMut, channel_id: &str, account: impl Into<String>) {
        let _account: String = account.into();

        let handshake_open = mock_ibc_channel_open_init(channel_id, APP_ORDER, IBC_APP_VERSION);
        // first we try to open with a valid handshake
        ibc_channel_open(deps.branch(), mock_env(), handshake_open).unwrap();

        // then we connect (with counter-party version set)
        let handshake_connect =
            mock_ibc_channel_connect_ack(channel_id, APP_ORDER, IBC_APP_VERSION);
        let res = ibc_channel_connect(deps.branch(), mock_env(), handshake_connect).unwrap();
        assert_eq!(res.messages.len(), 0);
        assert_eq!(res.events.len(), 1);
        assert_eq!(
            res.events[0],
            Event::new("ibc").add_attribute("channel", "connect"),
        );
    }

    //
    // Instantiate tests
    //

    #[test]
    fn instantiate_works() {
        let mut deps = mock_dependencies();

        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        let info = mock_info("creator", &[]);
        let res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(0, res.messages.len());

        let config: ConfigResponse =
            from_binary(&query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
        assert_eq!(
            config,
            ConfigResponse {
                min_round: TESTING_MIN_ROUND,
                incentive_amount: Uint128::new(1_000_000),
                incentive_denom: "unois".to_string(),
            }
        );
    }

    //
    // Execute tests
    //
    fn register_bot(deps: DepsMut, info: MessageInfo) {
        let register_bot_msg = ExecuteMsg::RegisterBot {
            moniker: "Best Bot".to_string(),
        };
        execute(deps, mock_env(), info, register_bot_msg).unwrap();
    }

    #[test]
    fn add_round_verifies_and_stores_randomness() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        let info = mock_info("anyone", &[]);
        register_bot(deps.as_mut(), info.to_owned());

        let msg = ExecuteMsg::AddRound {
            // curl -sS https://drand.cloudflare.com/public/72785
            round: 72785,
            previous_signature: HexBinary::from_hex("a609e19a03c2fcc559e8dae14900aaefe517cb55c840f6e69bc8e4f66c8d18e8a609685d9917efbfb0c37f058c2de88f13d297c7e19e0ab24813079efe57a182554ff054c7638153f9b26a60e7111f71a0ff63d9571704905d3ca6df0b031747").unwrap(),
            signature: HexBinary::from_hex("82f5d3d2de4db19d40a6980e8aa37842a0e55d1df06bd68bddc8d60002e8e959eb9cfa368b3c1b77d18f02a54fe047b80f0989315f83b12a74fd8679c4f12aae86eaf6ab5690b34f1fddd50ee3cc6f6cdf59e95526d5a5d82aaa84fa6f181e42").unwrap(),
        };
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        let response: BeaconResponse = from_binary(
            &query(deps.as_ref(), mock_env(), QueryMsg::Beacon { round: 72785 }).unwrap(),
        )
        .unwrap();
        assert_eq!(
            response.beacon.unwrap().randomness.to_hex(),
            "8b676484b5fb1f37f9ec5c413d7d29883504e5b669f604a1ce68b3388e9ae3d9"
        );
    }

    #[test]
    fn add_round_fails_when_round_too_low() {
        let mut deps = mock_dependencies();

        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        let info = mock_info("creator", &[]);
        let res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(0, res.messages.len());

        let ConfigResponse { min_round, .. } =
            from_binary(&query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
        assert_eq!(min_round, TESTING_MIN_ROUND);

        let msg = make_add_round_msg(9);
        let err = execute(deps.as_mut(), mock_env(), mock_info("anyone", &[]), msg).unwrap_err();
        assert!(matches!(
            err,
            ContractError::RoundTooLow {
                round: 9,
                min_round: TESTING_MIN_ROUND,
            }
        ));
    }

    #[test]
    fn unregistered_bot_does_not_get_incentives() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);

        let env = mock_env();
        let contract = env.contract.address;
        //add balance to the delegator contract
        deps.querier.update_balance(
            contract,
            vec![Coin {
                denom: "unois".to_string(),
                amount: Uint128::new(100_000_000),
            }],
        );

        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        let msg = ExecuteMsg::AddRound {
            // curl -sS https://drand.cloudflare.com/public/72785
            round: 72785,
            previous_signature: HexBinary::from_hex("a609e19a03c2fcc559e8dae14900aaefe517cb55c840f6e69bc8e4f66c8d18e8a609685d9917efbfb0c37f058c2de88f13d297c7e19e0ab24813079efe57a182554ff054c7638153f9b26a60e7111f71a0ff63d9571704905d3ca6df0b031747").unwrap(),
            signature: HexBinary::from_hex("82f5d3d2de4db19d40a6980e8aa37842a0e55d1df06bd68bddc8d60002e8e959eb9cfa368b3c1b77d18f02a54fe047b80f0989315f83b12a74fd8679c4f12aae86eaf6ab5690b34f1fddd50ee3cc6f6cdf59e95526d5a5d82aaa84fa6f181e42").unwrap(),
        };
        let info = mock_info("unregistered_bot", &[]);
        let response = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
        let randomness_attr = response
            .attributes
            .iter()
            .find(|Attribute { key, .. }| key == "randomness")
            .unwrap();
        assert_eq!(
            randomness_attr.value,
            "8b676484b5fb1f37f9ec5c413d7d29883504e5b669f604a1ce68b3388e9ae3d9"
        );
        assert_eq!(response.messages.len(), 0);
    }

    #[test]
    fn when_contract_does_not_have_enough_funds_no_bot_incentives_are_sent() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        //instantiate contract

        let env = mock_env();
        let contract = env.contract.address;
        //add balance to the delegator contract
        deps.querier.update_balance(
            contract,
            vec![Coin {
                denom: "unois".to_string(),
                amount: Uint128::new(10_000),
            }],
        );

        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        let msg = make_add_round_msg(72785);
        let info = mock_info("registered_bot", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let response = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
        let randomness_attr = response
            .attributes
            .iter()
            .find(|Attribute { key, .. }| key == "randomness")
            .unwrap();
        assert_eq!(
            randomness_attr.value,
            "8b676484b5fb1f37f9ec5c413d7d29883504e5b669f604a1ce68b3388e9ae3d9"
        );
        assert_eq!(response.messages.len(), 0)
    }

    #[test]
    fn only_top_x_bots_receive_incentive() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[Coin::new(100_000_000, "unois")]);
        let env = mock_env();
        let contract = env.contract.address;
        //add balance to the oracle contract
        deps.querier.update_balance(
            contract,
            vec![Coin {
                denom: "unois".to_string(),
                amount: Uint128::new(100_000_000),
            }],
        );

        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        let bot1 = "registered_bot1";
        let bot2 = "registered_bot2";
        let bot3 = "registered_bot3";
        let bot4 = "registered_bot4";
        let bot5 = "registered_bot5";
        let bot6 = "registered_bot6";
        let bot7 = "registered_bot7";

        register_bot(deps.as_mut(), mock_info(bot1, &[]));
        register_bot(deps.as_mut(), mock_info(bot2, &[]));
        register_bot(deps.as_mut(), mock_info(bot3, &[]));
        register_bot(deps.as_mut(), mock_info(bot4, &[]));
        register_bot(deps.as_mut(), mock_info(bot5, &[]));
        register_bot(deps.as_mut(), mock_info(bot6, &[]));
        register_bot(deps.as_mut(), mock_info(bot7, &[]));

        // Same msg for all submissions
        let msg = make_add_round_msg(72785);

        // 1st
        let info = mock_info(bot1, &[]);
        let response = execute(deps.as_mut(), mock_env(), info, msg.clone()).unwrap();
        assert_eq!(response.messages.len(), 1);

        // 2nd
        let info = mock_info(bot2, &[]);
        let response = execute(deps.as_mut(), mock_env(), info, msg.clone()).unwrap();
        assert_eq!(response.messages.len(), 1);

        // 3rd
        let info = mock_info(bot3, &[]);
        let response = execute(deps.as_mut(), mock_env(), info, msg.clone()).unwrap();
        assert_eq!(response.messages.len(), 1);

        // 4th
        let info = mock_info(bot4, &[]);
        let response = execute(deps.as_mut(), mock_env(), info, msg.clone()).unwrap();
        assert_eq!(response.messages.len(), 1);

        // 5th
        let info = mock_info(bot5, &[]);
        let response = execute(deps.as_mut(), mock_env(), info, msg.clone()).unwrap();
        assert_eq!(response.messages.len(), 1);

        // 6th
        let info = mock_info(bot6, &[]);
        let response = execute(deps.as_mut(), mock_env(), info, msg.clone()).unwrap();
        assert_eq!(response.messages.len(), 1);

        // 7th, here no message is emitted
        let info = mock_info(bot7, &[]);
        let response = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(response.messages.len(), 0);
    }

    #[test]
    fn unregistered_bot_can_add_round() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        let msg = make_add_round_msg(72785);
        let info = mock_info("unregistered_bot", &[]);
        let response = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
        let randomness_attr = response
            .attributes
            .iter()
            .find(|Attribute { key, .. }| key == "randomness")
            .unwrap();
        assert_eq!(
            randomness_attr.value,
            "8b676484b5fb1f37f9ec5c413d7d29883504e5b669f604a1ce68b3388e9ae3d9"
        );
    }

    #[test]
    fn add_round_fails_for_broken_signature() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        let info = mock_info("anyone", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let msg = ExecuteMsg::AddRound {
            // curl -sS https://drand.cloudflare.com/public/72785
            round: 72785,
            previous_signature: hex::decode("a609e19a03c2fcc559e8dae14900aaefe517cb55c840f6e69bc8e4f66c8d18e8a609685d9917efbfb0c37f058c2de88f13d297c7e19e0ab24813079efe57a182554ff054c7638153f9b26a60e7111f71a0ff63d9571704905d3ca6df0b031747").unwrap().into(),
            signature: hex::decode("3cc6f6cdf59e95526d5a5d82aaa84fa6f181e4").unwrap().into(), // broken signature
        };
        let result = execute(deps.as_mut(), mock_env(), info, msg);
        match result.unwrap_err() {
            ContractError::InvalidSignature {} => {}
            err => panic!("Unexpected error: {:?}", err),
        };
    }

    #[test]
    fn add_round_fails_for_invalid_signature() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        let msg = ExecuteMsg::AddRound {
            // curl -sS https://drand.cloudflare.com/public/72785
            round: 79999, // wrong round
            previous_signature: hex::decode("a609e19a03c2fcc559e8dae14900aaefe517cb55c840f6e69bc8e4f66c8d18e8a609685d9917efbfb0c37f058c2de88f13d297c7e19e0ab24813079efe57a182554ff054c7638153f9b26a60e7111f71a0ff63d9571704905d3ca6df0b031747").unwrap().into(),
            signature: hex::decode("82f5d3d2de4db19d40a6980e8aa37842a0e55d1df06bd68bddc8d60002e8e959eb9cfa368b3c1b77d18f02a54fe047b80f0989315f83b12a74fd8679c4f12aae86eaf6ab5690b34f1fddd50ee3cc6f6cdf59e95526d5a5d82aaa84fa6f181e42").unwrap().into(),
        };
        let result = execute(deps.as_mut(), mock_env(), mock_info("anon", &[]), msg);
        match result.unwrap_err() {
            ContractError::InvalidSignature {} => {}
            err => panic!("Unexpected error: {:?}", err),
        };

        let msg = ExecuteMsg::AddRound {
            // curl -sS https://drand.cloudflare.com/public/72785
            round: 72785,
            // wrong previous_signature
            previous_signature: hex::decode("cccccccccccccccc59e8dae14900aaefe517cb55c840f6e69bc8e4f66c8d18e8a609685d9917efbfb0c37f058c2de88f13d297c7e19e0ab24813079efe57a182554ff054c7638153f9b26a60e7111f71a0ff63d9571704905d3ca6df0b031747").unwrap().into(),
            signature: hex::decode("82f5d3d2de4db19d40a6980e8aa37842a0e55d1df06bd68bddc8d60002e8e959eb9cfa368b3c1b77d18f02a54fe047b80f0989315f83b12a74fd8679c4f12aae86eaf6ab5690b34f1fddd50ee3cc6f6cdf59e95526d5a5d82aaa84fa6f181e42").unwrap().into(),
        };
        let result = execute(deps.as_mut(), mock_env(), mock_info("anon", &[]), msg);
        match result.unwrap_err() {
            ContractError::InvalidSignature {} => {}
            err => panic!("Unexpected error: {:?}", err),
        };
    }

    #[test]
    fn add_round_succeeds_multiple_times() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        let msg = make_add_round_msg(72785);

        // Execute 1
        let info = mock_info("anyone", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let response = execute(deps.as_mut(), mock_env(), info, msg.clone()).unwrap();
        let randomness_attr = response
            .attributes
            .iter()
            .find(|Attribute { key, .. }| key == "randomness")
            .unwrap();
        assert_eq!(
            randomness_attr.value,
            "8b676484b5fb1f37f9ec5c413d7d29883504e5b669f604a1ce68b3388e9ae3d9"
        );

        // Execute 2
        let info = mock_info("someone else", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let response = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
        let randomness_attr = response
            .attributes
            .iter()
            .find(|Attribute { key, .. }| key == "randomness")
            .unwrap();
        assert_eq!(
            randomness_attr.value,
            "8b676484b5fb1f37f9ec5c413d7d29883504e5b669f604a1ce68b3388e9ae3d9"
        );
    }

    #[test]
    fn add_round_fails_when_same_bot_submits_multiple_times() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        let msg = make_add_round_msg(72785);

        // Execute A1
        let info = mock_info("bot_alice", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        execute(deps.as_mut(), mock_env(), info, msg.clone()).unwrap();
        // Execute B1
        let info = mock_info("bot_bob", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        execute(deps.as_mut(), mock_env(), info, msg.clone()).unwrap();

        // Execute A2
        let info = mock_info("bot_alice", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let err = execute(deps.as_mut(), mock_env(), info, msg.clone()).unwrap_err();
        assert!(matches!(err, ContractError::SubmissionExists));
        // Execute B2
        let info = mock_info("bot_alice", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
        assert!(matches!(err, ContractError::SubmissionExists));
    }

    #[test]
    fn add_round_processes_jobs() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        // Create one job
        let msg = mock_ibc_packet_recv(
            "foo",
            &RequestBeaconPacket {
                after: Timestamp::from_seconds(1660941090 - 1),
                job_id: "test 1".to_string(),
                sender: "my_dapp".to_string(),
            },
        )
        .unwrap();
        ibc_packet_receive(deps.as_mut(), mock_env(), msg).unwrap();

        // Previous round processes no job
        let msg = make_add_round_msg(2183668);
        let res = execute(deps.as_mut(), mock_env(), mock_info("anon", &[]), msg).unwrap();
        assert_eq!(res.messages.len(), 0);

        // Process one job
        let msg = make_add_round_msg(2183669);
        let res = execute(deps.as_mut(), mock_env(), mock_info("anon", &[]), msg).unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].gas_limit, None);
        assert!(matches!(
            res.messages[0].msg,
            CosmosMsg::Ibc(IbcMsg::SendPacket { .. })
        ));

        // Create five job
        for i in 0..3 {
            let msg = mock_ibc_packet_recv(
                "foo",
                &RequestBeaconPacket {
                    after: Timestamp::from_seconds(1660941120 - 1),
                    job_id: format!("test {i}"),
                    sender: "my_dapp".to_string(),
                },
            )
            .unwrap();
            ibc_packet_receive(deps.as_mut(), mock_env(), msg).unwrap();
        }

        // Process five jobs
        let msg = make_add_round_msg(2183670);
        let res = execute(deps.as_mut(), mock_env(), mock_info("anon", &[]), msg).unwrap();
        assert_eq!(res.messages.len(), 3);
        assert_eq!(res.messages[0].gas_limit, None);
        assert_eq!(res.messages[1].gas_limit, None);
        assert_eq!(res.messages[2].gas_limit, None);
        assert!(matches!(
            res.messages[0].msg,
            CosmosMsg::Ibc(IbcMsg::SendPacket { .. })
        ));
        assert!(matches!(
            res.messages[1].msg,
            CosmosMsg::Ibc(IbcMsg::SendPacket { .. })
        ));
        assert!(matches!(
            res.messages[2].msg,
            CosmosMsg::Ibc(IbcMsg::SendPacket { .. })
        ));

        // Create 7 job
        for i in 0..7 {
            let msg = mock_ibc_packet_recv(
                "foo",
                &RequestBeaconPacket {
                    after: Timestamp::from_seconds(1660941150 - 1),
                    job_id: format!("test {i}"),
                    sender: "my_dapp".to_string(),
                },
            )
            .unwrap();
            ibc_packet_receive(deps.as_mut(), mock_env(), msg).unwrap();
        }

        // Process first 3 jobs
        let msg = make_add_round_msg(2183671);
        let res = execute(deps.as_mut(), mock_env(), mock_info("anon1", &[]), msg).unwrap();
        assert_eq!(res.messages.len(), 3);

        // Process next 3 jobs
        let msg = make_add_round_msg(2183671);
        let res = execute(deps.as_mut(), mock_env(), mock_info("anon2", &[]), msg).unwrap();
        assert_eq!(res.messages.len(), 3);

        // Process last 1 jobs
        let msg = make_add_round_msg(2183671);
        let res = execute(deps.as_mut(), mock_env(), mock_info("anon3", &[]), msg).unwrap();
        assert_eq!(res.messages.len(), 1);

        // No jobs left for later submissions
        let msg = make_add_round_msg(2183671);
        let res = execute(deps.as_mut(), mock_env(), mock_info("anon4", &[]), msg).unwrap();
        assert_eq!(res.messages.len(), 0);
    }

    #[test]
    fn register_bot_works_for_updates() {
        let mut deps = mock_dependencies();
        let bot_addr = "bot_addr".to_string();

        // first registration

        let info = mock_info(&bot_addr, &[]);
        let register_bot_msg = ExecuteMsg::RegisterBot {
            moniker: "Nickname1".to_string(),
        };
        execute(deps.as_mut(), mock_env(), info, register_bot_msg).unwrap();
        let BotResponse { bot } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::Bot {
                    address: bot_addr.clone(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        let bot = bot.unwrap();
        assert_eq!(
            bot,
            QueriedBot {
                moniker: "Nickname1".to_string(),
                address: Addr::unchecked(&bot_addr),
                rounds_added: 0,
            }
        );

        // re-register

        let info = mock_info(&bot_addr, &[]);
        let register_bot_msg = ExecuteMsg::RegisterBot {
            moniker: "Another nickname".to_string(),
        };
        execute(deps.as_mut(), mock_env(), info, register_bot_msg).unwrap();
        let BotResponse { bot } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::Bot {
                    address: bot_addr.clone(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        let bot = bot.unwrap();
        assert_eq!(
            bot,
            QueriedBot {
                moniker: "Another nickname".to_string(),
                address: Addr::unchecked(&bot_addr),
                rounds_added: 0,
            }
        );
    }

    //
    // Query tests
    //

    #[test]
    fn query_beacons_asc_works() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        let info = mock_info("anyone", &[]);
        register_bot(deps.as_mut(), info);
        add_test_rounds(deps.as_mut(), "anyone");

        // Unlimited
        let BeaconsResponse { beacons } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::BeaconsAsc {
                    start_after: None,
                    limit: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let response_rounds = beacons.iter().map(|b| b.round).collect::<Vec<u64>>();
        assert_eq!(response_rounds, [72785, 72786, 72787]);

        // Limit 2
        let BeaconsResponse { beacons } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::BeaconsAsc {
                    start_after: None,
                    limit: Some(2),
                },
            )
            .unwrap(),
        )
        .unwrap();
        let response_rounds = beacons.iter().map(|b| b.round).collect::<Vec<u64>>();
        assert_eq!(response_rounds, [72785, 72786]);

        // After 0
        let BeaconsResponse { beacons } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::BeaconsAsc {
                    start_after: Some(0),
                    limit: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let response_rounds = beacons.iter().map(|b| b.round).collect::<Vec<u64>>();
        assert_eq!(response_rounds, [72785, 72786, 72787]);

        // After 72785
        let BeaconsResponse { beacons } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::BeaconsAsc {
                    start_after: Some(72785),
                    limit: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let response_rounds = beacons.iter().map(|b| b.round).collect::<Vec<u64>>();
        assert_eq!(response_rounds, [72786, 72787]);

        // After 72787
        let BeaconsResponse { beacons } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::BeaconsAsc {
                    start_after: Some(72787),
                    limit: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let response_rounds = beacons.iter().map(|b| b.round).collect::<Vec<u64>>();
        assert_eq!(response_rounds, Vec::<u64>::new());
    }

    #[test]
    fn query_beacons_desc_works() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        let info = mock_info("anyone", &[]);
        register_bot(deps.as_mut(), info);
        add_test_rounds(deps.as_mut(), "anyone");

        // Unlimited
        let BeaconsResponse { beacons } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::BeaconsDesc {
                    start_after: None,
                    limit: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let response_rounds = beacons.iter().map(|b| b.round).collect::<Vec<u64>>();
        assert_eq!(response_rounds, [72787, 72786, 72785]);

        // Limit 2
        let BeaconsResponse { beacons } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::BeaconsDesc {
                    start_after: None,
                    limit: Some(2),
                },
            )
            .unwrap(),
        )
        .unwrap();
        let response_rounds = beacons.iter().map(|b| b.round).collect::<Vec<u64>>();
        assert_eq!(response_rounds, [72787, 72786]);

        // After 99999
        let BeaconsResponse { beacons } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::BeaconsDesc {
                    start_after: Some(99999),
                    limit: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let response_rounds = beacons.iter().map(|b| b.round).collect::<Vec<u64>>();
        assert_eq!(response_rounds, [72787, 72786, 72785]);

        // After 72787
        let BeaconsResponse { beacons } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::BeaconsDesc {
                    start_after: Some(72787),
                    limit: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let response_rounds = beacons.iter().map(|b| b.round).collect::<Vec<u64>>();
        assert_eq!(response_rounds, [72786, 72785]);

        // After 72785
        let BeaconsResponse { beacons } = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::BeaconsDesc {
                    start_after: Some(72785),
                    limit: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let response_rounds = beacons.iter().map(|b| b.round).collect::<Vec<u64>>();
        assert_eq!(response_rounds, Vec::<u64>::new());
    }

    #[test]
    fn query_submissions_works() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        // Address order is not submission order
        let bot1 = "beta1";
        let bot2 = "gamma2";
        let bot3 = "alpha3";

        let info = mock_info(bot1, &[]);
        register_bot(deps.as_mut(), info);
        add_test_rounds(deps.as_mut(), bot1);

        // No submissions
        let response: SubmissionsResponse = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::Submissions { round: 72777 },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(response.round, 72777);
        assert_eq!(response.submissions, Vec::<_>::new());

        // One submission
        let response: SubmissionsResponse = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::Submissions { round: 72785 },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(response.round, 72785);
        assert_eq!(
            response.submissions,
            [QueriedSubmission {
                bot: Addr::unchecked(bot1),
                time: Timestamp::from_nanos(1571797419879305533),
            }]
        );

        add_test_rounds(deps.as_mut(), bot2);

        // Two submissions
        let response: SubmissionsResponse = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::Submissions { round: 72785 },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(response.round, 72785);
        assert_eq!(
            response.submissions,
            [
                QueriedSubmission {
                    bot: Addr::unchecked(bot1),
                    time: Timestamp::from_nanos(1571797419879305533),
                },
                QueriedSubmission {
                    bot: Addr::unchecked(bot2),
                    time: Timestamp::from_nanos(1571797419879305533),
                },
            ]
        );

        add_test_rounds(deps.as_mut(), bot3);

        // Three submissions
        let response: SubmissionsResponse = from_binary(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::Submissions { round: 72785 },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(response.round, 72785);
        assert_eq!(
            response.submissions,
            [
                QueriedSubmission {
                    bot: Addr::unchecked(bot1),
                    time: Timestamp::from_nanos(1571797419879305533),
                },
                QueriedSubmission {
                    bot: Addr::unchecked(bot2),
                    time: Timestamp::from_nanos(1571797419879305533),
                },
                QueriedSubmission {
                    bot: Addr::unchecked(bot3),
                    time: Timestamp::from_nanos(1571797419879305533),
                },
            ]
        );
    }

    #[test]
    fn query_job_stats_works() {
        let mut deps = mock_dependencies();

        let info = mock_info("creator", &[]);
        register_bot(deps.as_mut(), info.to_owned());
        let msg = InstantiateMsg {
            min_round: TESTING_MIN_ROUND,
            incentive_amount: Uint128::new(1_000_000),
            incentive_denom: "unois".to_string(),
        };
        instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

        fn job_stats(deps: Deps, round: u64) -> JobStatsResponse {
            from_binary(&query(deps, mock_env(), QueryMsg::JobStats { round }).unwrap()).unwrap()
        }

        // No jobs by default
        assert_eq!(
            job_stats(deps.as_ref(), 2183669),
            JobStatsResponse {
                round: 2183669,
                processed: 0,
                unprocessed: 0,
            }
        );

        // Create one job
        let msg = mock_ibc_packet_recv(
            "foo",
            &RequestBeaconPacket {
                after: Timestamp::from_seconds(1660941090 - 1),
                job_id: "test 1".to_string(),
                sender: "my_dapp".to_string(),
            },
        )
        .unwrap();
        ibc_packet_receive(deps.as_mut(), mock_env(), msg).unwrap();

        // One unprocessed job
        assert_eq!(
            job_stats(deps.as_ref(), 2183669),
            JobStatsResponse {
                round: 2183669,
                processed: 0,
                unprocessed: 1,
            }
        );

        let msg = make_add_round_msg(2183669);
        execute(deps.as_mut(), mock_env(), mock_info("bot", &[]), msg).unwrap();

        // 1 processed job, no unprocessed jobs
        assert_eq!(
            job_stats(deps.as_ref(), 2183669),
            JobStatsResponse {
                round: 2183669,
                processed: 1,
                unprocessed: 0,
            }
        );

        // New job for existing round gets processed immediately
        let msg = mock_ibc_packet_recv(
            "foo",
            &RequestBeaconPacket {
                after: Timestamp::from_seconds(1660941090 - 1),
                job_id: "test 2".to_string(),
                sender: "my_dapp".to_string(),
            },
        )
        .unwrap();
        ibc_packet_receive(deps.as_mut(), mock_env(), msg).unwrap();

        // 2 processed job, no unprocessed jobs
        assert_eq!(
            job_stats(deps.as_ref(), 2183669),
            JobStatsResponse {
                round: 2183669,
                processed: 2,
                unprocessed: 0,
            }
        );

        // Create 20 jobs
        for i in 0..20 {
            let msg = mock_ibc_packet_recv(
                "foo",
                &RequestBeaconPacket {
                    after: Timestamp::from_seconds(1660941150 - 1),
                    job_id: format!("job {i}"),
                    sender: "my_dapp".to_string(),
                },
            )
            .unwrap();
            ibc_packet_receive(deps.as_mut(), mock_env(), msg).unwrap();
        }

        // 20 unprocessed
        assert_eq!(
            job_stats(deps.as_ref(), 2183671),
            JobStatsResponse {
                round: 2183671,
                processed: 0,
                unprocessed: 20,
            }
        );

        // process some
        let msg = make_add_round_msg(2183671);
        execute(deps.as_mut(), mock_env(), mock_info("bot", &[]), msg).unwrap();

        // Some processed, rest unprocessed
        assert_eq!(
            job_stats(deps.as_ref(), 2183671),
            JobStatsResponse {
                round: 2183671,
                processed: 3,
                unprocessed: 17,
            }
        );
    }

    //
    // IBC tests
    //

    #[test]
    fn ibc_channel_open_checks_version_and_order() {
        let mut deps = setup();

        // All good
        let valid_handshake = mock_ibc_channel_open_try("channel-12", APP_ORDER, IBC_APP_VERSION);
        ibc_channel_open(deps.as_mut(), mock_env(), valid_handshake).unwrap();

        // Wrong order
        let wrong_order = mock_ibc_channel_open_try("channel-12", BAD_APP_ORDER, IBC_APP_VERSION);
        let res = ibc_channel_open(deps.as_mut(), mock_env(), wrong_order).unwrap_err();
        assert!(matches!(res, ContractError::ChannelError(..)));

        // Wrong version
        let wrong_version = mock_ibc_channel_open_try("channel-12", APP_ORDER, "another version");
        let res = ibc_channel_open(deps.as_mut(), mock_env(), wrong_version).unwrap_err();
        assert!(matches!(res, ContractError::ChannelError(..)));
    }

    #[test]
    fn proper_handshake_flow() {
        let mut deps = setup();
        let channel_id = "channel-1234";

        // first we try to open with a valid handshake
        let handshake_open = mock_ibc_channel_open_init(channel_id, APP_ORDER, IBC_APP_VERSION);
        ibc_channel_open(deps.as_mut(), mock_env(), handshake_open).unwrap();

        // then we connect (with counter-party version set)
        let handshake_connect =
            mock_ibc_channel_connect_ack(channel_id, APP_ORDER, IBC_APP_VERSION);
        let _res = ibc_channel_connect(deps.as_mut(), mock_env(), handshake_connect).unwrap();
    }

    #[test]
    fn check_close_channel() {
        let mut deps = setup();

        let channel_id = "channel-123";
        let account = "acct-123";

        // register the channel
        connect(deps.as_mut(), channel_id, account);
        // assign it some funds
        let funds = vec![coin(123456, "uatom"), coin(7654321, "tgrd")];
        deps.querier.update_balance(account, funds);

        // close the channel
        let channel = mock_ibc_channel_close_init(channel_id, APP_ORDER, IBC_APP_VERSION);
        let _res = ibc_channel_close(deps.as_mut(), mock_env(), channel).unwrap();
    }

    //
    // Other
    //

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
