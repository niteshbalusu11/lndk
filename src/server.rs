use crate::lnd::{get_lnd_client, get_network, Creds, LndCfg};
use crate::lndk_offers::{get_destination, validate_amount};
use crate::{
    lndkrpc, Bolt12InvoiceString, OfferError, OfferHandler, PayOfferParams, TLS_CERT_FILENAME,
    TLS_KEY_FILENAME,
};
use bitcoin::secp256k1::PublicKey;
use lightning::blinded_path::payment::BlindedPaymentPath;
use lightning::blinded_path::{Direction, IntroductionNode};
use lightning::ln::channelmanager::PaymentId;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::offers::offer::Offer;
use lightning::sign::EntropySource;
use lightning::util::ser::Writeable;
use lndkrpc::offers_server::Offers;
use lndkrpc::{
    Bolt12InvoiceContents, DecodeInvoiceRequest, FeatureBit, GetInvoiceRequest, GetInvoiceResponse,
    PayInvoiceRequest, PayInvoiceResponse, PayOfferRequest, PayOfferResponse, PaymentHash,
    PaymentPaths,
};
use rcgen::{generate_simple_self_signed, CertifiedKey, Error as RcgenError};
use std::error::Error;
use std::fmt::Display;
use std::fs::{metadata, set_permissions, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tonic::metadata::MetadataMap;
use tonic::transport::Identity;
use tonic::{Request, Response, Status};
use tonic_lnd::lnrpc::GetInfoRequest;
pub struct LNDKServer {
    offer_handler: Arc<OfferHandler>,
    #[allow(dead_code)]
    node_id: PublicKey,
    // The LND tls cert we need to establish a connection with LND.
    lnd_cert: String,
    address: String,
}

impl LNDKServer {
    pub async fn new(
        offer_handler: Arc<OfferHandler>,
        node_id: &str,
        lnd_cert: String,
        address: String,
    ) -> Self {
        Self {
            offer_handler,
            node_id: PublicKey::from_str(node_id).unwrap(),
            lnd_cert,
            address,
        }
    }
}

#[tonic::async_trait]
impl Offers for LNDKServer {
    async fn pay_offer(
        &self,
        request: Request<PayOfferRequest>,
    ) -> Result<Response<PayOfferResponse>, Status> {
        log::info!("Received a request: {:?}", request.get_ref());

        let metadata = request.metadata();
        let macaroon = check_auth_metadata(metadata)?;
        let creds = Creds::String {
            cert: self.lnd_cert.clone(),
            macaroon,
        };
        let lnd_cfg = LndCfg::new(self.address.clone(), creds);
        let mut client = get_lnd_client(lnd_cfg)
            .map_err(|e| Status::unavailable(format!("Couldn't connect to lnd: {e}")))?;

        let inner_request = request.get_ref();
        let offer = Offer::from_str(&inner_request.offer).map_err(|e| {
            Status::invalid_argument(format!(
                "The provided offer was invalid. Please provide a valid offer in bech32 format,
                i.e. starting with 'lno'. Error: {e:?}"
            ))
        })?;

        let destination = get_destination(&offer).await.map_err(|e| {
            Status::internal(format!(
                "Internal error: Couldn't get destination from offer: {e:?}"
            ))
        })?;
        let reply_path = None;
        let info = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .expect("failed to get info")
            .into_inner();
        let network = get_network(info)
            .await
            .map_err(|e| Status::internal(format!("{e:?}")))?;

        let cfg = PayOfferParams {
            offer,
            amount: inner_request.amount,
            payer_note: inner_request.payer_note.clone(),
            network,
            client,
            destination,
            reply_path,
            response_invoice_timeout: inner_request.response_invoice_timeout,
        };

        let payment = match self.offer_handler.pay_offer(cfg).await {
            Ok(payment) => {
                log::info!("Payment succeeded.");
                payment
            }
            Err(e) => match e {
                OfferError::InvalidAmount(e) => {
                    return Err(Status::invalid_argument(e.to_string()))
                }
                OfferError::InvalidCurrency => {
                    return Err(Status::invalid_argument(format!("{e}")))
                }
                _ => return Err(Status::internal(format!("Internal error: {e}"))),
            },
        };

        let reply = PayOfferResponse {
            payment_preimage: payment.payment_preimage,
        };

        Ok(Response::new(reply))
    }

    async fn decode_invoice(
        &self,
        request: Request<DecodeInvoiceRequest>,
    ) -> Result<Response<Bolt12InvoiceContents>, Status> {
        log::info!("Received a request: {:?}", request.get_ref());

        let invoice_string: Bolt12InvoiceString = request.get_ref().invoice.clone().into();
        let invoice = Bolt12Invoice::try_from(invoice_string)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        let reply: Bolt12InvoiceContents = generate_bolt12_invoice_contents(&invoice);
        Ok(Response::new(reply))
    }

    async fn get_invoice(
        &self,
        request: Request<GetInvoiceRequest>,
    ) -> Result<Response<GetInvoiceResponse>, Status> {
        log::info!("Received a request: {:?}", request.get_ref());

        let metadata = request.metadata();
        let macaroon = check_auth_metadata(metadata)?;
        let creds = Creds::String {
            cert: self.lnd_cert.clone(),
            macaroon,
        };
        let lnd_cfg = LndCfg::new(self.address.clone(), creds);
        let mut client = get_lnd_client(lnd_cfg)
            .map_err(|e| Status::unavailable(format!("Couldn't connect to lnd: {e}")))?;

        let inner_request = request.get_ref();
        let offer = Offer::from_str(&inner_request.offer).map_err(|e| {
            Status::invalid_argument(format!(
                "The provided offer was invalid. Please provide a valid offer in bech32 format,
                i.e. starting with 'lno'. Error: {e:?}"
            ))
        })?;

        let destination = get_destination(&offer)
            .await
            .map_err(|e| Status::unavailable(format!("Couldn't find destination: {e}")))?;
        let reply_path = None;

        let info = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .expect("failed to get info")
            .into_inner();
        let network = get_network(info)
            .await
            .map_err(|e| Status::internal(format!("{e:?}")))?;

        let cfg = PayOfferParams {
            offer,
            amount: inner_request.amount,
            payer_note: inner_request.payer_note.clone(),
            network,
            client,
            destination,
            reply_path,
            response_invoice_timeout: inner_request.response_invoice_timeout,
        };

        let (invoice, _, payment_id) = match self.offer_handler.get_invoice(cfg).await {
            Ok(invoice) => {
                log::info!("Invoice request succeeded.");
                invoice
            }
            Err(e) => match e {
                OfferError::InvalidAmount(e) => {
                    return Err(Status::invalid_argument(e.to_string()))
                }
                OfferError::InvalidCurrency => {
                    return Err(Status::invalid_argument(format!("{e}")))
                }
                _ => return Err(Status::internal(format!("Internal error: {e}"))),
            },
        };

        // We need to remove the payment from our tracking map now.
        {
            let mut active_payments = self.offer_handler.active_payments.lock().unwrap();
            active_payments.remove(&payment_id);
        }

        let reply: GetInvoiceResponse = GetInvoiceResponse {
            invoice_hex_str: encode_invoice_as_hex(&invoice)?,
            invoice_contents: Some(generate_bolt12_invoice_contents(&invoice)),
        };

        Ok(Response::new(reply))
    }

    async fn pay_invoice(
        &self,
        request: Request<PayInvoiceRequest>,
    ) -> Result<Response<PayInvoiceResponse>, Status> {
        log::info!("Received a request: {:?}", request.get_ref());

        let metadata = request.metadata();
        let macaroon = check_auth_metadata(metadata)?;
        let creds = Creds::String {
            cert: self.lnd_cert.clone(),
            macaroon,
        };
        let lnd_cfg = LndCfg::new(self.address.clone(), creds);
        let client = get_lnd_client(lnd_cfg)
            .map_err(|e| Status::unavailable(format!("Couldn't connect to lnd: {e}")))?;

        let inner_request = request.get_ref();
        let invoice_string: Bolt12InvoiceString = inner_request.invoice.clone().into();
        let invoice = Bolt12Invoice::try_from(invoice_string).map_err(|e| {
            Status::invalid_argument(format!(
                "The provided invoice was invalid. Please provide a valid invoice in hex format.
                Error: {e:?}"
            ))
        })?;

        let amount = match validate_amount(invoice.amount().as_ref(), inner_request.amount).await {
            Ok(amount) => amount,
            Err(e) => return Err(Status::invalid_argument(e.to_string())),
        };
        let payment_id = PaymentId(self.offer_handler.messenger_utils.get_secure_random_bytes());
        let invoice = match self
            .offer_handler
            .pay_invoice(client, amount, &invoice, payment_id)
            .await
        {
            Ok(invoice) => {
                log::info!("Invoice paid.");
                invoice
            }
            Err(e) => return Err(Status::internal(format!("Error paying invoice: {e}"))),
        };

        let reply = PayInvoiceResponse {
            payment_preimage: invoice.payment_preimage,
        };

        Ok(Response::new(reply))
    }
}

// We need to check that the client passes in a tls cert pem string, hexadecimal macaroon,
// and address, so they can connect to LND.
fn check_auth_metadata(metadata: &MetadataMap) -> Result<String, Status> {
    let macaroon = match metadata.get("macaroon") {
        Some(macaroon_hex) => macaroon_hex
            .to_str()
            .map_err(|e| {
                Status::invalid_argument(format!("Invalid macaroon string provided: {e}"))
            })?
            .to_string(),
        _ => {
            return Err(Status::unauthenticated(
                "No LND macaroon provided: Make sure to provide macaroon in request metadata",
            ))
        }
    };

    Ok(macaroon)
}

/// An error that occurs when generating TLS credentials.
#[derive(Debug)]
pub enum CertificateGenFailure {
    RcgenError(RcgenError),
    IoError(std::io::Error),
}

impl Display for CertificateGenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertificateGenFailure::RcgenError(e) => {
                write!(f, "Error generating TLS certificate: {e:?}")
            }
            CertificateGenFailure::IoError(e) => write!(f, "IO error: {e:?}"),
        }
    }
}

impl Error for CertificateGenFailure {}

// If a tls cert/key pair doesn't already exist, generate_tls_creds creates the tls cert/key pair
// required to secure connections to LNDK's gRPC server. By default they are stored in ~/.lndk.
pub fn generate_tls_creds(
    data_dir: PathBuf,
    tls_ips_string: Option<String>,
) -> Result<(), CertificateGenFailure> {
    let cert_path = data_dir.join(TLS_CERT_FILENAME);
    let key_path = data_dir.join(TLS_KEY_FILENAME);
    let tls_ips = collect_tls_ips(tls_ips_string);

    // Did we have to generate a new key? In that case we also need to regenerate the certificate.
    if !key_path.exists() || !cert_path.exists() {
        log::debug!("Generating fresh TLS credentials in {data_dir:?}");
        let mut subject_alt_names = vec!["localhost".to_string()];
        if let Some(ips) = tls_ips {
            for ip in ips {
                subject_alt_names.push(ip);
            }
        };

        let CertifiedKey { cert, key_pair } = generate_simple_self_signed(subject_alt_names)
            .map_err(CertificateGenFailure::RcgenError)?;

        // Create the tls files. Make sure the key is user-readable only:
        let mut file = File::create(&key_path).map_err(CertificateGenFailure::IoError)?;
        let mut perms = metadata(&key_path)
            .map_err(CertificateGenFailure::IoError)?
            .permissions();
        perms.set_mode(0o600);
        set_permissions(&key_path, perms).map_err(CertificateGenFailure::IoError)?;

        file.write_all(key_pair.serialize_pem().as_bytes())
            .map_err(CertificateGenFailure::IoError)?;
        drop(file);

        std::fs::write(&cert_path, cert.pem()).map_err(CertificateGenFailure::IoError)?;
    };

    Ok(())
}

// Read the existing tls credentials from disk.
pub fn read_tls(data_dir: PathBuf) -> Result<Identity, std::io::Error> {
    let cert = std::fs::read_to_string(data_dir.join(TLS_CERT_FILENAME))?;
    let key = std::fs::read_to_string(data_dir.join(TLS_KEY_FILENAME))?;

    Ok(Identity::from_pem(cert, key))
}

// The user first passes in the tls ips as a comma-deliminated string into LNDK. Here we turn that
// string into a Vec.
fn collect_tls_ips(tls_ips_str: Option<String>) -> Option<Vec<String>> {
    tls_ips_str.map(|tls_ips_str| tls_ips_str.split(',').map(|str| str.to_owned()).collect())
}

fn generate_bolt12_invoice_contents(invoice: &Bolt12Invoice) -> lndkrpc::Bolt12InvoiceContents {
    Bolt12InvoiceContents {
        chain: invoice.chain().to_string(),
        quantity: invoice.quantity(),
        amount_msats: invoice.amount_msats(),
        description: invoice
            .description()
            .map(|description| description.to_string()),
        payment_hash: Some(PaymentHash {
            hash: invoice.payment_hash().encode(),
        }),
        created_at: invoice.created_at().as_secs() as i64,
        relative_expiry: invoice.relative_expiry().as_secs(),
        node_id: Some(convert_public_key(&invoice.signing_pubkey())),
        signature: invoice.signature().to_string(),
        payment_paths: extract_payment_paths(invoice),
        features: convert_invoice_features(invoice.invoice_features().clone()),
        payer_note: invoice
            .payer_note()
            .map(|payer_note| payer_note.to_string()),
    }
}

fn encode_invoice_as_hex(invoice: &Bolt12Invoice) -> Result<String, Status> {
    let mut buffer = Vec::new();
    invoice
        .write(&mut buffer)
        .map_err(|e| Status::internal(format!("Error serializing invoice: {e}")))?;
    Ok(hex::encode(buffer))
}

fn extract_payment_paths(invoice: &Bolt12Invoice) -> Vec<PaymentPaths> {
    invoice
        .payment_paths()
        .iter()
        .map(|path| PaymentPaths {
            blinded_pay_info: Some(convert_blinded_pay_info(&path.payinfo)),
            blinded_path: Some(convert_blinded_path(path)),
        })
        .collect()
}

fn convert_public_key(native_pub_key: &PublicKey) -> lndkrpc::PublicKey {
    let pub_key_bytes = native_pub_key.encode();
    lndkrpc::PublicKey { key: pub_key_bytes }
}

fn convert_invoice_features(_features: impl std::fmt::Debug) -> Vec<i32> {
    vec![FeatureBit::MppOpt as i32]
}

fn convert_blinded_pay_info(
    native_info: &lightning::blinded_path::payment::BlindedPayInfo,
) -> lndkrpc::BlindedPayInfo {
    lndkrpc::BlindedPayInfo {
        fee_base_msat: native_info.fee_base_msat,
        fee_proportional_millionths: native_info.fee_proportional_millionths,
        cltv_expiry_delta: native_info.cltv_expiry_delta as u32,
        htlc_minimum_msat: native_info.htlc_minimum_msat,
        htlc_maximum_msat: native_info.htlc_maximum_msat,
        ..Default::default()
    }
}

fn convert_blinded_path(native_info: &BlindedPaymentPath) -> lndkrpc::BlindedPath {
    let introduction_node = match native_info.introduction_node() {
        IntroductionNode::NodeId(pubkey) => lndkrpc::IntroductionNode {
            node_id: Some(convert_public_key(pubkey)),
            directed_short_channel_id: None,
        },
        IntroductionNode::DirectedShortChannelId(direction, scid) => {
            let rpc_direction = match direction {
                Direction::NodeOne => lndkrpc::Direction::NodeOne,
                Direction::NodeTwo => lndkrpc::Direction::NodeTwo,
            };

            lndkrpc::IntroductionNode {
                node_id: None,
                directed_short_channel_id: Some(lndkrpc::DirectedShortChannelId {
                    direction: rpc_direction.into(),
                    scid: *scid,
                }),
            }
        }
    };

    lndkrpc::BlindedPath {
        introduction_node: Some(introduction_node),
        blinding_point: Some(convert_public_key(&native_info.blinding_point())),
        blinded_hops: native_info
            .blinded_hops()
            .iter()
            .map(|hop| lndkrpc::BlindedHop {
                blinded_node_id: Some(convert_public_key(&hop.blinded_node_id)),
                encrypted_payload: hop.encrypted_payload.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_tls_ips() {
        // Test that it returns a vector of one element if only one ip is provided.
        let tls_ips_str = Some("192.168.0.1".to_string());
        let tls_ips = collect_tls_ips(tls_ips_str);
        assert!(tls_ips.is_some());
        assert!(tls_ips.as_ref().unwrap().len() == 1);

        // If no ip is provided, collect_tls_ips should return None.
        assert!(collect_tls_ips(None).is_none());

        // If two ips are provided, a vector of length two should be returned.
        let tls_ips_str = Some("192.168.0.1,192.168.0.3".to_string());
        let tls_ips = collect_tls_ips(tls_ips_str);
        assert!(tls_ips.is_some());
        assert!(tls_ips.as_ref().unwrap().len() == 2);
    }
}
