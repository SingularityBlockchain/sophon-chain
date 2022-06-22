use std::{collections::HashMap, convert::TryFrom};

use inflector::Inflector;
use serde_json::Value;
use thiserror::Error;

use solana_sdk::{instruction::InstructionError, pubkey::Pubkey, system_program, sysvar};

use crate::{
    parse_bpf_loader::parse_bpf_upgradeable_loader,
    parse_config::parse_config,
    parse_nonce::parse_nonce,
    parse_stake::parse_stake,
    parse_sysvar::parse_sysvar,
    parse_token::{parse_token, spl_token_id_v2_0},
    parse_vote::parse_vote,
};

lazy_static! {
    static ref BPF_UPGRADEABLE_LOADER_PROGRAM_ID: Pubkey = solana_sdk::bpf_loader_upgradeable::id();
    static ref CONFIG_PROGRAM_ID: Pubkey = solana_config_program::id();
    static ref STAKE_PROGRAM_ID: Pubkey = solana_stake_program::id();
    static ref SYSTEM_PROGRAM_ID: Pubkey = system_program::id();
    static ref SYSVAR_PROGRAM_ID: Pubkey = sysvar::id();
    static ref TOKEN_PROGRAM_ID: Pubkey = spl_token_id_v2_0();
    static ref VOTE_PROGRAM_ID: Pubkey = solana_vote_program::id();
    static ref SOPHON_ACCOUNT_PROGRAM_ID: Pubkey = sophon_account_program::id();
    static ref SOPHON_RELYING_PARTY_PROGRAM_ID: Pubkey = sophon_relying_party_program::id();
    pub static ref PARSABLE_PROGRAM_IDS: HashMap<Pubkey, ParsableAccount> = {
        let mut m = HashMap::new();
        m.insert(
            *BPF_UPGRADEABLE_LOADER_PROGRAM_ID,
            ParsableAccount::BpfUpgradeableLoader,
        );
        m.insert(*CONFIG_PROGRAM_ID, ParsableAccount::Config);
        m.insert(*SYSTEM_PROGRAM_ID, ParsableAccount::Nonce);
        m.insert(*TOKEN_PROGRAM_ID, ParsableAccount::SplToken);
        m.insert(*STAKE_PROGRAM_ID, ParsableAccount::Stake);
        m.insert(*SYSVAR_PROGRAM_ID, ParsableAccount::Sysvar);
        m.insert(*VOTE_PROGRAM_ID, ParsableAccount::Vote);
        m.insert(*SOPHON_ACCOUNT_PROGRAM_ID, ParsableAccount::SophonAccount);
        m.insert(
            *SOPHON_RELYING_PARTY_PROGRAM_ID,
            ParsableAccount::SophonRelyingParty,
        );
        m
    };
}

#[derive(Error, Debug)]
pub enum ParseAccountError {
    #[error("{0:?} account not parsable")]
    AccountNotParsable(ParsableAccount),

    #[error("Program not parsable")]
    ProgramNotParsable,

    #[error("Additional data required to parse: {0}")]
    AdditionalDataMissing(String),

    #[error("Instruction error")]
    InstructionError(#[from] InstructionError),

    #[error("Serde json error")]
    SerdeJsonError(#[from] serde_json::error::Error),
}

impl From<sophon_account_program::ParseError> for ParseAccountError {
    fn from(err: sophon_account_program::ParseError) -> Self {
        match err {
            sophon_account_program::ParseError::AccountNotParsable => {
                Self::AccountNotParsable(ParsableAccount::SophonAccount)
            }
        }
    }
}

impl From<sophon_relying_party_program::ParseError> for ParseAccountError {
    fn from(err: sophon_relying_party_program::ParseError) -> Self {
        match err {
            sophon_relying_party_program::ParseError::AccountNotParsable => {
                Self::AccountNotParsable(ParsableAccount::SophonRelyingParty)
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ParsedAccount {
    pub program: String,
    pub parsed: Value,
    pub space: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParsableAccount {
    BpfUpgradeableLoader,
    Config,
    Nonce,
    SplToken,
    Stake,
    Sysvar,
    Vote,
    SophonAccount,
    SophonRelyingParty,
}

#[derive(Default)]
pub struct AccountAdditionalData {
    pub spl_token_decimals: Option<u8>,
}

pub fn parse_account_data(
    pubkey: &Pubkey,
    program_id: &Pubkey,
    data: &[u8],
    additional_data: Option<AccountAdditionalData>,
) -> Result<ParsedAccount, ParseAccountError> {
    let program_name = PARSABLE_PROGRAM_IDS
        .get(program_id)
        .ok_or(ParseAccountError::ProgramNotParsable)?;
    let additional_data = additional_data.unwrap_or_default();
    let parsed_json = match program_name {
        ParsableAccount::BpfUpgradeableLoader => {
            serde_json::to_value(parse_bpf_upgradeable_loader(data)?)?
        }
        ParsableAccount::Config => serde_json::to_value(parse_config(data, pubkey)?)?,
        ParsableAccount::Nonce => serde_json::to_value(parse_nonce(data)?)?,
        ParsableAccount::SplToken => {
            serde_json::to_value(parse_token(data, additional_data.spl_token_decimals)?)?
        }
        ParsableAccount::Stake => serde_json::to_value(parse_stake(data)?)?,
        ParsableAccount::Sysvar => serde_json::to_value(parse_sysvar(data, pubkey)?)?,
        ParsableAccount::Vote => serde_json::to_value(parse_vote(data)?)?,
        ParsableAccount::SophonAccount => {
            serde_json::to_value(sophon_account_program::SophonAccountType::try_from(data)?)?
        }
        ParsableAccount::SophonRelyingParty => serde_json::to_value(
            sophon_relying_party_program::RelyingPartyData::try_from(data)?,
        )?,
    };
    Ok(ParsedAccount {
        program: format!("{:?}", program_name).to_kebab_case(),
        parsed: parsed_json,
        space: data.len() as u64,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use solana_sdk::nonce::{
        state::{Data, Versions},
        State,
    };
    use solana_vote_program::vote_state::{VoteState, VoteStateVersions};

    #[test]
    fn test_parse_account_data() {
        let account_pubkey = solana_sdk::pubkey::new_rand();
        let other_program = solana_sdk::pubkey::new_rand();
        let data = vec![0; 4];
        assert!(parse_account_data(&account_pubkey, &other_program, &data, None).is_err());

        let vote_state = VoteState::default();
        let mut vote_account_data: Vec<u8> = vec![0; VoteState::size_of()];
        let versioned = VoteStateVersions::new_current(vote_state);
        VoteState::serialize(&versioned, &mut vote_account_data).unwrap();
        let parsed = parse_account_data(
            &account_pubkey,
            &solana_vote_program::id(),
            &vote_account_data,
            None,
        )
        .unwrap();
        assert_eq!(parsed.program, "vote".to_string());
        assert_eq!(parsed.space, VoteState::size_of() as u64);

        let nonce_data = Versions::new_current(State::Initialized(Data::default()));
        let nonce_account_data = bincode::serialize(&nonce_data).unwrap();
        let parsed = parse_account_data(
            &account_pubkey,
            &system_program::id(),
            &nonce_account_data,
            None,
        )
        .unwrap();
        assert_eq!(parsed.program, "nonce".to_string());
        assert_eq!(parsed.space, State::size() as u64);
    }
}
