use std::{fmt, num::ParseIntError, str::FromStr, borrow::Cow};

// The default rust 'oui' crate doesn't search efficiently, and we can't use it memory-optimized ways.
//
// Namely, for each OUI lookup by MAC address:
// * It searches through every OUI prefix instead of performing a binary search
// * It always allocates strings for vendor description return values, instead of returning string slices.
//
// Other OUI-based crates seem focused on vendor mailing addresses rather than OUI descriptors.

use eui48::{MacAddress, EUI48LEN};

use crate::mac::MacStyle;

#[derive(thiserror::Error, Debug)]
pub enum ParseMacError {
    #[error("MAC address has a bad character length: {0:?}")]
    InvalidLength(String),
    #[error("Found an invalid character in MAC {0:?}: {1:?}")]
    InvalidCharacter(String, char),
}

// rolling our own parsing - the built-in mac addr parsing from the eui48 crate is way too slow for DB use.
// see: https://github.com/abaumhauer/eui48/pull/32
// note that our mac addr zero-extension logic wouldn't port over into that PR too well, so use homegrown

pub fn parse_mac_addr(s: &str) -> Result<eui48::MacAddress, ParseMacError> {
    parse_mac_addr_extend(s, false)
}
pub fn parse_mac_addr_extend(
    mut s: &str,
    zero_extend: bool,
) -> Result<eui48::MacAddress, ParseMacError> {
    let mut raw = smallstr::SmallString::<[u8; 12]>::new();
    if s.starts_with("0x") {
        s = &s[2..];
    }
    for c in s.chars() {
        if matches!(c, 'A'..='F' | 'a'..='f' | '0'..='9') {
            if raw.len() + 1 > raw.capacity() {
                return Err(ParseMacError::InvalidLength(s.to_owned()));
            }
            raw.push(c);
        } else if !matches!(c, '-' | '.' | ':') {
            return Err(ParseMacError::InvalidCharacter(s.to_owned(), c));
        }
    }

    if zero_extend {
        // fill in the end if we were only given the front (like parsing an OUI/prefix)
        const ZEROS: [char; 12] = ['0'; 12];
        raw.extend(&ZEROS[raw.len()..]);
    }

    if raw.len() < 12 {
        return Err(ParseMacError::InvalidLength(s.to_owned()));
    }

    debug_assert_eq!(raw.len(), 12);

    let mac_int: u64 =
        u64::from_str_radix(raw.as_str(), 16).expect("prevalidated that all chars are hexidecimal");

    Ok(Oui::from_int(mac_int).unwrap().as_mac())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OuiMeta<S> {
    short: S,
    long: Option<S>,
    comment: Option<S>,
}
impl<S> OuiMeta<S> {
    pub const fn manuf(&self) -> &S {
        &self.short
    }
    pub const fn manuf_long(&self) -> Option<&S> {
        self.long.as_ref()
    }
    pub const fn comment(&self) -> Option<&S> {
        self.comment.as_ref()
    }
}
impl<'a> OuiMeta<&'a str> {
    pub fn to_owned(&self) -> OuiMeta<String> {
        OuiMeta {
            short: self.short.to_owned(),
            long: self.long.map(|s| s.to_owned()),
            comment: self.comment.map(|s| s.to_owned()),
        }
    }
}
impl OuiMeta<String> {
    pub fn as_ref(&self) -> OuiMeta<&'_ str> {
        OuiMeta {
            short: self.short.as_str(),
            long: self.long.as_deref(),
            comment: self.comment.as_deref(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseOuiError {
    #[error(transparent)]
    MacParsing(#[from] ParseMacError),
    #[error("Unable to parse prefix length from OUI string {1:?}")]
    PrefixLengthParsing(#[source] ParseIntError, String),
    #[error("Parsed an invalid OUI prefix length. Expected values are within range [24, 48]. Got {1} from source prefix {0:?}")]
    PrefixLengthValue(u8, Cow<'static, str>),
    #[error("Attempted to create an OUI/MAC address from a 64-bit integer, but value was out of range. Got 0x{0:>016x}")]
    InvalidIntegerValue(u64),
}

#[derive(Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
pub struct Oui {
    address: u64,
    length: u8,
}
impl Oui {
    pub const fn as_mac(self) -> MacAddress {
        let mac_raw_long = u64::to_be_bytes(self.address);
        let mut mac_raw = [0u8; 6];
        mac_raw[0] = mac_raw_long[2];
        mac_raw[1] = mac_raw_long[3];
        mac_raw[2] = mac_raw_long[4];
        mac_raw[3] = mac_raw_long[5];
        mac_raw[4] = mac_raw_long[6];
        mac_raw[5] = mac_raw_long[7];

        // copy_from_slice is not const
        // mac_raw.copy_from_slice(&mac_raw_long[2..]);

        MacAddress::new(mac_raw)
    }
    pub const fn mask(&self) -> u64 {
        ((1 << self.length) - 1) << (8 * EUI48LEN - self.length as usize)
    }
    pub const fn length(&self) -> u8 {
        self.length
    }
    pub const fn with_length(&self, len: u8) -> Result<Oui, ParseOuiError> {
        if len > 48 {
            return Err(ParseOuiError::PrefixLengthValue(len, Cow::Borrowed("Oui::set_length")));
        }
        let mut local = *self;
        local.length = len;
        Ok(local)
    }

    pub const fn contains(&self, other: &Oui) -> bool {
        // eprintln!("Oui::contains({:?}, {:?} (self mask: {:b}))", self, other, self.mask());
        if self.length > other.length {
            return false;
        }
        other.address & self.mask() == self.address
    }

    /// Creates an OUI with length of 48 from an array of bytes. The last byte of the MAC should be the first byte in the array.
    /// 
    /// In other words:
    /// `"AA-BB-CC-DD-EE-FF"` is `0x0000AABBCCDDEEFF` is `[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]`
    pub const fn from_array(mac: [u8; 6]) -> Oui {
        // there is currently no way to const-ly retrieve MAC bytes from a eui48::MacAddress

        let mut mac_bytes_u64 = [0u8; 8];

        mac_bytes_u64[2] = mac[0];
        mac_bytes_u64[3] = mac[1];
        mac_bytes_u64[4] = mac[2];
        mac_bytes_u64[5] = mac[3];
        mac_bytes_u64[6] = mac[4];
        mac_bytes_u64[7] = mac[5];

        let mac_int = u64::from_be_bytes(mac_bytes_u64);
        Oui {
            address: mac_int,
            length: 48,
        }
    }
    pub fn from_addr(mac: MacAddress) -> Oui {
        // MacAddress::as_bytes() is not const
        Oui::from_array(mac.as_bytes().try_into().unwrap())
    }

    /// Returns the MAC address as a u64.
    ///
    /// This places the address in the least significant digits: `aa:bb:cc:dd:ee:ff` would be `0x0000aabbccddeeff`
    pub const fn as_int(&self) -> u64 {
        self.address
    }

    /// Converts a 64-bit integer into a structured OUI with a length of 48 bits.
    ///
    /// Returns Err(ParseOuiError::InvalidIntegerValue(_)) if the address is over 0x0000FFFF_FFFFFF
    pub const fn from_int(address: u64) -> Result<Oui, ParseOuiError> {
        if address > 0x0000_FFFF_FFFF_FFFF {
            return Err(ParseOuiError::InvalidIntegerValue(address));
        }
        Ok(Oui {
            address,
            length: 6 * 8,
        })
    }
}
impl FromStr for Oui {
    type Err = ParseOuiError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (oui, length) = match s.split_once('/') {
            None => (s, 24),
            Some((oui, slen)) => (
                oui,
                slen.parse::<u8>()
                    .map_err(|e| ParseOuiError::PrefixLengthParsing(e, s.to_owned()))?,
            ),
        };

        if !(24..=48).contains(&length) {
            return Err(ParseOuiError::PrefixLengthValue(length, Cow::from(s.to_owned())));
        }

        let oui_mac = parse_mac_addr_extend(oui, true).unwrap();
        let mut address = Oui::from_addr(oui_mac);
        address.length = length;

        Ok(address)
    }
}
impl fmt::Debug for Oui {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let formatted = MacStyle::Colon.format(self.as_mac(), false);
        let fstr = formatted.as_str();

        // alternate flag signals to always use extended form
        match (self.length, f.alternate()) {
            (24, false) => f.write_str(&fstr[..8]),
            _ => f.write_fmt(format_args!("{}/{}", fstr, self.length)),
        }
    }
}

#[test]
fn check_smallstr_size() {
    use smallstr::SmallString;

    // this test is kept more as a reminder that these aren't 'free'
    // a String's main node is 3*WORDSIZE + ALLOCATION
    // for a 14B MAC on 64-bit CPU, that would be ~38 bytes, assuming no alloc padding

    // the advantage of this approach is data locality - smallstring
    // keeps the data on the stack, along with other local data
    // so there isn't need to randomly access memory for heap, etc

    // let's keep those caches hot, folks

    assert_eq!(32, std::mem::size_of::<SmallString<[u8; 14]>>());
    assert_eq!(32, std::mem::size_of::<SmallString<[u8; 17]>>());
    assert_eq!(32, std::mem::size_of::<SmallString<[u8; 19]>>());
    assert_eq!(40, std::mem::size_of::<SmallString<[u8; 25]>>());
}

/// In-memory OUI prefix database
///
/// Lookups should generally be O(log n), as we perform a binary search to locate an OUI prefix, when given a username
#[derive(Debug, Clone)]
pub struct OuiDb(Vec<(Oui, OuiMeta<String>)>);

lazy_static::lazy_static! {
    pub static ref EMBEDDED_DB: OuiDb = {
        OuiDb::parse_from_string(OuiDb::WIRESHARK_OUI_DB_EMBEDDED).expect("failure parsing embedded wireshark oui database")
    };
}

#[derive(Debug, thiserror::Error)]
pub enum ParseOuiDbError {
    #[error("error parsing oui in db record (line {0}: {2:?})")]
    OuiParsing(usize, #[source] ParseOuiError, String),
    #[error("invalid number of fields in oui db record, expected [2, 4] got {1} (line {0}: {2:?})")]
    BadFieldCount(usize, usize, String),

    #[cfg(debug_assertions)]
    #[error("entries with duplicate prefix's exist within the OUI database")]
    DuplicatedEntries,
}

impl OuiDb {
    /// The latest copy of Wireshark's OUI database at compile time.
    ///
    /// Latest copy is available here: https://gitlab.com/wireshark/wireshark/raw/master/manuf
    pub const WIRESHARK_OUI_DB_EMBEDDED: &str =
        include_str!(concat!(env!("OUT_DIR"), "/wireshark_oui_db.txt"));

    // TODO: pub fn parse_from_reader<R: BufRead>(txt: R) -> Result<OuiDb, DbParsingError>

    /// Parse a file in the format of Wireshark's OUI database into memory.
    ///
    /// Wireshark's reference OUI database can be found here: https://gitlab.com/wireshark/wireshark/raw/master/manuf
    pub fn parse_from_string(txt: &str) -> Result<OuiDb, ParseOuiDbError> {
        let mut v: Vec<(Oui, OuiMeta<String>)> = txt
            .split('\n')
            .enumerate()
            .map(|(lnum, l)| (lnum, l.trim()))
            .filter(|(_, l)| !(l.is_empty() || l.starts_with('#')))
            .map(|(lnum, l)| {
                let mut _fields = [""; 8];
                let fields: &[&str] = {
                    let mut len = 0;
                    l.split('\t')
                        .filter(|f| f.len() > 1)
                        .enumerate()
                        .for_each(|(i, part)| {
                            len = i + 1;
                            _fields[i] = part.trim();
                        });
                    &_fields[..len]
                };
                if !(2..=4).contains(&fields.len()) {
                    return Err(ParseOuiDbError::BadFieldCount(
                        lnum,
                        fields.len(),
                        l.to_owned(),
                    ));
                }
                let ouispec: Oui = fields[0]
                    .parse()
                    .map_err(|e| ParseOuiDbError::OuiParsing(lnum, e, l.to_owned()))?;
                let short = fields[1];
                let long = fields.get(2).copied();
                let comment = fields.get(3).map(|s| s.trim_matches('#').trim());
                Ok((
                    ouispec,
                    OuiMeta {
                        short,
                        long,
                        comment,
                    }
                    .to_owned(),
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;

        // sort it for binary searching later
        v.sort_by_key(|(k, _v)| *k);

        #[cfg(debug_assertions)]
        {
            // no need to error on this if running in release mode
            // the sourced DB shouldn't have any and it's not worth erroring over anyway
            // this is primarily for diagnostics
            let prededup_len = v.len();
            v.dedup_by_key(|(k, _v)| *k);
            if prededup_len != v.len() {
                return Err(ParseOuiDbError::DuplicatedEntries);
            }
        }

        // let dbg_str: String = v.iter()
        //     .enumerate()
        //     .map(|(i, (o, om))| format!("{:>05}\t{:>012x}/{}\t{:?}\t{:?}\n", i, o.address, o.length, o, om))
        //     .collect();
        // std::fs::write("oui_db_dump.txt", dbg_str).unwrap();

        Ok(OuiDb(v))
    }

    pub fn search_entry(&self, mac: MacAddress) -> Option<(Oui, OuiMeta<&str>)> {
        let as_oui = Oui::from_addr(mac);
        // eprintln!("searching MAC {:?} with OUI {:?}", mac, as_oui);
        let base_i = match self.0.binary_search_by_key(&as_oui, |(o, _om)| *o) {
            Ok(i) => i, // exact match
            Err(i) => {
                // should be n-above our desired entry
                // should /be/ our desired entry if the prefix is long
                // may have to iterate towards zero if we are within a longer prefix, and must match for the parent prefix
                // subtract zero to go to the lower end of our match
                i - 1
            }
        };
        let mut i = base_i;

        loop {
            let (o, om) = self.0.get(i)?;
            if o.contains(&as_oui) {
                // this is our prefix
                return Some((*o, om.as_ref()));
            } else if !o.contains(&as_oui) && o.length <= 24 {
                // we reached a top-level-prefix (/24) that doesn't contain us - we have none
                return None;
            } else {
                // continue searching upwards for a containing prefix until we find one, or find a top-level that we arent' in
                i -= 1;
            }
        }
    }

    pub fn raw_prefixes(&self) -> impl Iterator<Item = (Oui, OuiMeta<&str>)> {
        self.0.iter().map(|(o, om)| (*o, om.as_ref()))
    }
    pub fn search_prefix(&self, mac: MacAddress) -> Option<Oui> {
        self.search_entry(mac).map(|(p, _)| p)
    }
    pub fn search(&self, mac: MacAddress) -> Option<OuiMeta<&str>> {
        self.search_entry(mac).map(|(_, om)| om)
    }
}
impl FromStr for OuiDb {
    type Err = ParseOuiDbError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_from_string(s)
    }
}

#[test]
fn embedded_db_builds() {
    OuiDb::parse_from_string(OuiDb::WIRESHARK_OUI_DB_EMBEDDED).unwrap();
}

#[test]
fn match_no_long_name() {
    // 00:00:17	Oracle
    let mac = parse_mac_addr("00:00:17:aa:bb:cc").unwrap();
    assert_eq!(
        EMBEDDED_DB.search(mac),
        Some(OuiMeta {
            short: "Oracle",
            long: None,
            comment: None,
        })
    );
}

#[test]
fn match_prefix_zeros() {
    // 00:00:17	Oracle
    let mac = parse_mac_addr("00:00:00:00:00:00").unwrap();
    assert_eq!(
        EMBEDDED_DB.search(mac),
        Some(OuiMeta {
            short: "00:00:00",
            long: Some("Officially Xerox, but 0:0:0:0:0:0 is more common"),
            comment: None,
        })
    );
}

#[test]
fn match_prefix_exact() {
    // 2C:23:3A	HewlettP	Hewlett Packard
    let mac = parse_mac_addr("2c:23:3a:00:00:00").unwrap();
    assert_eq!(
        EMBEDDED_DB.search(mac),
        Some(OuiMeta {
            short: "HewlettP",
            long: Some("Hewlett Packard"),
            comment: None,
        })
    );
}

#[test]
fn match_prefix_basic() {
    // 2C:23:3A	HewlettP	Hewlett Packard
    let mac = parse_mac_addr("2c:23:3a:aa:bb:cc").unwrap();
    assert_eq!(
        EMBEDDED_DB.search(mac),
        Some(OuiMeta {
            short: "HewlettP",
            long: Some("Hewlett Packard"),
            comment: None,
        })
    );
}

#[test]
fn match_prefix_extended() {
    // 8C:47:6E:30:00:00/28	Shanghai	Shanghai Satellite Communication Technology Co.,Ltd
    let mac = parse_mac_addr("8c:47:6e:3a:bb:cc").unwrap();
    assert_eq!(
        EMBEDDED_DB.search(mac),
        Some(OuiMeta {
            short: "Shanghai",
            long: Some("Shanghai Satellite Communication Technology Co.,Ltd"),
            comment: None,
        })
    );
}

#[test]
fn match_commented() {
    // 08:00:87	XyplexTe	Xyplex	# terminal servers
    let mac = parse_mac_addr("08:00:87:aa:bb:cc").unwrap();
    assert_eq!(
        EMBEDDED_DB.search(mac),
        Some(OuiMeta {
            short: "XyplexTe",
            long: Some("Xyplex"),
            comment: Some("terminal servers"),
        })
    );
}

#[test]
fn match_unicode() {
    // 8C:1F:64:CB:20:00/36	DyncirSo	Dyncir Soluções Tecnológicas Ltda
    let mac = parse_mac_addr("8c:1f:64:cb:2b:cc").unwrap();
    assert_eq!(
        EMBEDDED_DB.search(mac),
        Some(OuiMeta {
            short: "DyncirSo",
            long: Some("Dyncir Soluções Tecnológicas Ltda"),
            comment: None,
        })
    );
}

#[test]
fn resolve_mac_to_superprefix_when_missing_subprefix() {
    // 2C:27:9E	IEEERegi	IEEE Registration Authority
    // is split into /28, without a 2C:27:9E:F0:00:00/28 member
    let mac = parse_mac_addr("2c:27:9e:fa:bb:cc").unwrap();
    assert_eq!(
        EMBEDDED_DB.search(mac),
        Some(OuiMeta {
            short: "IEEERegi",
            long: Some("IEEE Registration Authority"),
            comment: None,
        })
    );
}

#[test]
fn match_none() {
    // B0:C5:59	SamsungE	Samsung Electronics Co.,Ltd
    // B0:C5:CA	IEEERegi	IEEE Registration Authority
    let mac = parse_mac_addr("b0:c5:5a:aa:bb:cc").unwrap();
    assert_eq!(EMBEDDED_DB.search(mac), None);
}
