use crate::ContactType;
use clarity::Address;
use std::net::Ipv4Addr;
use std::time::SystemTime;

/// Contains all the data you need for an American mailing address
/// hopefully also compatible with a few other countries
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct MailingAddress {
    /// full string country name including spaces
    pub country: String,
    /// postal code, in whatever the local format is
    pub postal_code: String,
    /// State, country may not contain states so optional
    pub state: Option<String>,
    pub city: String,
    pub street: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
/// This struct contains details about the users billing address
/// name, etc. It does not duplicate ContactType and does not store
/// direct contact info like phone or email
pub struct BillingDetails {
    /// The users first name
    pub user_first_name: String,
    /// The users last name
    pub user_last_name: String,
    /// The mailing address of this installation, assumed to be in whatever
    /// format the local nation has for addresses. Optional as this install
    /// may not have a formal mailing address
    pub mailing_address: MailingAddress,
}

/// Struct for storing details about this user installation. This particular
/// struct exists in the settings on the router because it has to be persisted
/// long enough to make it to the operator tools, once it's been uploaded though
/// it has no reason to hand around and is mostly dead weight in the config. The
/// question is if we want to delete it or manage it somehow.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct InstallationDetails {
    /// The CPE ip of this client. This field seems straightforward but actually
    /// has quite a bit of optionality. What if the user is connected via l2 bridge
    /// (for example a cable, or fiber) in that case this could be None. If the client
    /// is multihomed which ip is the client antenna and which one is the relay antenna?
    /// That can be decided randomly without any problems I think.
    pub client_antenna_ip: Option<Ipv4Addr>,
    /// A list of addresses for relay antennas, this could include sectors and/or
    /// point to point links going downstream. If the vec is empty there are no
    /// relay antennas
    pub relay_antennas: Vec<Ipv4Addr>,
    /// A list of addresses for light client antennas. The vec can of course
    /// be empty representing no phone client antennas.
    pub phone_client_antennas: Vec<Ipv4Addr>,
    /// The address of this installation, this has no structure and should
    /// simply be displayed. Depending on the country address formats will
    /// be very different and we might even only have GPS points
    /// will only exist if mailing address over in contact info is blank
    pub physical_address: Option<String>,
    /// Description of the installation and equipment at the
    /// location
    pub equipment_details: String,
    /// Time of install, this is set by the operator tools when it accepts
    /// the value because the router system clocks may be problematic.
    pub install_date: Option<SystemTime>,
}

/// This struct carries info to the operator tools
/// to perform the registration request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WyreReservationRequestCarrier {
    /// the actual amount the user is requesting to deposit
    pub amount: f32,
    pub address: Address,
    pub contact_info: ContactType,
    pub billing_details: BillingDetails,
}

/// The exact struct for sending to this endpoint
///https://docs.sendwyre.com/docs/wallet-order-reservations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WyreReservationRequest {
    pub amount: f32,
    #[serde(rename = "sourceCurrency")]
    pub source_currency: String,
    #[serde(rename = "destCurrency")]
    pub dest_currency: String,
    pub dest: String,
    #[serde(rename = "firstName")]
    pub first_name: String,
    #[serde(rename = "lastName")]
    pub last_name: String,
    pub city: String,
    pub state: String,
    pub country: String,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub street1: String,
    #[serde(rename = "postalCode")]
    pub postal_code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WyreReservationResponse {
    url: String,
    reservation: String,
}
