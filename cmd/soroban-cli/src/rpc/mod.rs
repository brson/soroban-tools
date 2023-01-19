use jsonrpsee_core::{self, client::ClientT, rpc_params};
use jsonrpsee_http_client::{types, HeaderMap, HttpClient, HttpClientBuilder};
use soroban_env_host::xdr::{Error as XdrError, LedgerKey, TransactionEnvelope, WriteXdr};
use std::{
    collections,
    time::{Duration, Instant},
};
use tokio::time::sleep;

const VERSION: Option<&str> = option_env!("CARGO_PKG_VERSION");

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("xdr processing error: {0}")]
    Xdr(#[from] XdrError),
    #[error("jsonrpc error: {0}")]
    JsonRpc(#[from] jsonrpsee_core::Error),
    #[error("json decoding error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("transaction submission failed")]
    TransactionSubmissionFailed,
    #[error("expected transaction status: {0}")]
    UnexpectedTransactionStatus(String),
    #[error("transaction submission timeout")]
    TransactionSubmissionTimeout,
    #[error("transaction simulation failed: {0}")]
    TransactionSimulationFailed(String),
}

// TODO: this should also be used by serve
#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct GetAccountResponse {
    pub id: String,
    pub sequence: String,
    // TODO: add balances
}

// TODO: this should also be used by serve
#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct SendTransactionResponse {
    pub id: String,
    pub status: String,
    // TODO: add error
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct TransactionStatusResult {
    pub xdr: String,
}

// TODO: this should also be used by serve
#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct GetTransactionStatusResponse {
    pub id: String,
    pub status: String,
    #[serde(
        rename = "envelopeXdr",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub envelope_xdr: Option<String>,
    #[serde(rename = "resultXdr", skip_serializing_if = "Option::is_none", default)]
    pub result_xdr: Option<String>,
    #[serde(
        rename = "resultMetaXdr",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub result_meta_xdr: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub results: Vec<TransactionStatusResult>,
}

// TODO: this should also be used by serve
#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct GetContractDataResponse {
    pub xdr: String,
    // TODO: add lastModifiedLedgerSeq and latestLedger
}

// TODO: this should also be used by serve
#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct GetLedgerEntryResponse {
    pub xdr: String,
    // TODO: add lastModifiedLedgerSeq and latestLedger
}

// TODO: this should also be used by serve
#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct Cost {
    #[serde(rename = "cpuInsns")]
    pub cpu_insns: String,
    #[serde(rename = "memBytes")]
    pub mem_bytes: String,
}
#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct SimulateTransactionResponse {
    pub footprint: String,
    pub cost: Cost,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    // TODO: add results and latestLedger
}

pub type GetEventsResponse = Vec<Event>;

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct Event {
    #[serde(rename = "type")]
    pub event_type: String,

    pub ledger: String,
    #[serde(rename = "ledgerClosedAt")]
    pub ledger_closed_at: String,

    pub id: String,
    #[serde(rename = "pagingToken")]
    pub paging_token: String,

    #[serde(rename = "contractId")]
    pub contract_id: String,
    pub topic: Vec<String>,
    pub value: EventValue,
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct EventValue {
    pub xdr: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, clap::ArgEnum)]
pub enum EventType {
    All,
    Contract,
    System,
}

pub struct Client {
    base_url: String,
}

impl Client {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_string(),
        }
    }

    fn client(&self) -> Result<HttpClient, Error> {
        let url = self.base_url.clone();
        let mut headers = HeaderMap::new();
        headers.insert("X-Client-Name", "soroban-cli".parse().unwrap());
        let version = VERSION.unwrap_or("devel");
        headers.insert("X-Client-Version", version.parse().unwrap());
        // TODO: We should consider migrating the server subcommand to jsonrpsee
        Ok(HttpClientBuilder::default()
            .set_headers(headers)
            .build(url)?)
    }

    pub async fn get_account(&self, account_id: &str) -> Result<GetAccountResponse, Error> {
        Ok(self
            .client()?
            .request("getAccount", rpc_params![account_id])
            .await?)
    }

    pub async fn send_transaction(
        &self,
        tx: &TransactionEnvelope,
    ) -> Result<Vec<TransactionStatusResult>, Error> {
        let client = self.client()?;
        let SendTransactionResponse { id, status } = client
            .request("sendTransaction", rpc_params![tx.to_xdr_base64()?])
            .await
            .map_err(|_| Error::TransactionSubmissionFailed)?;

        if status == "error" {
            return Err(Error::TransactionSubmissionFailed);
        }
        // even if status == "success" we need to query the transaction status in order to get the result

        // Poll the transaction status
        let start = Instant::now();
        loop {
            let response = self.get_transaction_status(&id).await?;
            match response.status.as_str() {
                "success" => {
                    // TODO: the caller should probably be printing this
                    eprintln!("{}", response.status);
                    return Ok(response.results);
                }
                "error" => {
                    // TODO: provide a more elaborate error
                    return Err(Error::TransactionSubmissionFailed);
                }
                "pending" => (),
                _ => {
                    return Err(Error::UnexpectedTransactionStatus(response.status));
                }
            };
            let duration = start.elapsed();
            // TODO: parameterize the timeout instead of using a magic constant
            if duration.as_secs() > 10 {
                return Err(Error::TransactionSubmissionTimeout);
            }
            sleep(Duration::from_secs(1)).await;
        }
    }

    pub async fn simulate_transaction(
        &self,
        tx: &TransactionEnvelope,
    ) -> Result<SimulateTransactionResponse, Error> {
        let base64_tx = tx.to_xdr_base64()?;
        let response: SimulateTransactionResponse = self
            .client()?
            .request("simulateTransaction", rpc_params![base64_tx])
            .await?;
        match response.error {
            None => Ok(response),
            Some(e) => Err(Error::TransactionSimulationFailed(e)),
        }
    }

    pub async fn get_transaction_status(
        &self,
        tx_id: &str,
    ) -> Result<GetTransactionStatusResponse, Error> {
        Ok(self
            .client()?
            .request("getTransactionStatus", rpc_params![tx_id])
            .await?)
    }

    pub async fn get_ledger_entry(&self, key: LedgerKey) -> Result<GetLedgerEntryResponse, Error> {
        let base64_key = key.to_xdr_base64()?;
        Ok(self
            .client()?
            .request("getLedgerEntry", rpc_params![base64_key])
            .await?)
    }

    pub async fn get_events(
        &self,
        start_ledger: u32,
        end_ledger: u32,
        event_type: Option<EventType>,
        contract_ids: &[String],
        topics: &[String],
        limit: Option<usize>,
    ) -> Result<Option<GetEventsResponse>, Error> {
        let mut filters = serde_json::Map::new();

        event_type
            .and_then(|t| match t {
                EventType::All => None, // all is the default, so avoid incl. the param
                EventType::Contract => Some("contract"),
                EventType::System => Some("system"),
            })
            .map(|t| filters.insert("type".to_string(), t.into()));

        filters.insert("topics".to_string(), topics.into());
        filters.insert("contractIds".to_string(), contract_ids.into());

        let mut pagination = serde_json::Map::new();
        if let Some(limit) = limit {
            pagination.insert("limit".to_string(), limit.into());
        }
        // TODO: cursor

        let mut object = collections::BTreeMap::<&str, jsonrpsee_core::JsonValue>::new();
        object.insert("startLedger", start_ledger.to_string().into());
        object.insert("endLedger", end_ledger.to_string().into());
        object.insert("filters", vec![filters].into());
        object.insert("pagination", pagination.into());

        Ok(self
            .client()?
            .request("getEvents", Some(types::ParamsSer::Map(object)))
            .await?)
    }
}