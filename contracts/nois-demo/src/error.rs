use thiserror::Error;

use cosmwasm_std::StdError;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("Proxy address is not valid")]
    InvalidProxyAddress,

    #[error("Unauthorized Receive execution")]
    UnauthorizedReceive,

    #[error("Received invalid randomness")]
    InvalidRandomness,
}
