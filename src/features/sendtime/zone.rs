//! Each person's time zone: the profile's own `timezone` property when it's a real IANA zone, else
//! one derived from what we know about where they are (region, US zip, country, phone number), else
//! the account's. Only signals that pin down one zone are used: a country that spans several zones
//! says nothing on its own, and phone numbers are read only from the country code or, in North
//! America, the area code.

use std::collections::HashMap;
use std::str::FromStr;

use chrono::{Offset, TimeZone, Utc};
use chrono_tz::Tz;
use once_cell::sync::Lazy;
use serde_json::Value;

use crate::models::Profile;

/// Where a person's zone came from, for reports and the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Profile,
    Region,
    Zip,
    Country,
    Phone,
    Account,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Profile => "profile",
            Source::Region => "region",
            Source::Zip => "zip",
            Source::Country => "country",
            Source::Phone => "phone",
            Source::Account => "account",
        }
    }
}

/// The person's time zone and where it came from.
pub fn resolve(profile: &Profile, account_tz: Tz) -> (Tz, Source) {
    resolve_parts(&profile.properties, profile.phone_number.as_deref(), account_tz)
}

/// [`resolve`] from a profile's properties and phone number (for bulk reads that skip the full row).
pub fn resolve_parts(props: &Value, phone: Option<&str>, account_tz: Tz) -> (Tz, Source) {
    let text = |keys: &[&str]| keys.iter().find_map(|k| props.get(*k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()));
    if let Some(tz) = text(&["timezone", "$timezone", "time_zone"]).and_then(parse_zone) {
        return (tz, Source::Profile);
    }
    let country = text(&["country", "$country", "country_code"]).and_then(country_code);
    if let Some(tz) = text(&["region", "$region", "state", "province"]).and_then(|r| region_zone(country, r)) {
        return (tz, Source::Region);
    }
    if country == Some("US") {
        if let Some(tz) = text(&["zip", "$zip", "postal_code", "zip_code"]).and_then(us_zip_zone) {
            return (tz, Source::Zip);
        }
    }
    if let Some(tz) = country.and_then(country_zone) {
        return (tz, Source::Country);
    }
    if let Some(tz) = phone.and_then(phone_zone) {
        return (tz, Source::Phone);
    }
    (account_tz, Source::Account)
}

/// An IANA zone name (old names such as Asia/Calcutta included). Fixed offsets and "UTC"-style
/// abbreviations other than UTC itself are not zones people live in, so they don't count.
pub fn parse_zone(s: &str) -> Option<Tz> {
    let tz = Tz::from_str(s.trim()).ok()?;
    let name = tz.name();
    (name.contains('/') || name == "UTC").then_some(tz)
}

/// ISO 3166 alpha-2 code for a country given as a code or a common English name.
fn country_code(raw: &str) -> Option<&'static str> {
    let up = raw.trim().to_ascii_uppercase();
    let named = match up.as_str() {
        "USA" | "UNITED STATES" | "UNITED STATES OF AMERICA" | "U.S." | "U.S.A." => Some("US"),
        "CANADA" => Some("CA"),
        "AUSTRALIA" => Some("AU"),
        "UK" | "UNITED KINGDOM" | "GREAT BRITAIN" | "ENGLAND" | "SCOTLAND" | "WALES" => Some("GB"),
        "INDIA" => Some("IN"),
        "NEPAL" => Some("NP"),
        "MEXICO" => Some("MX"),
        "BRAZIL" => Some("BR"),
        "GERMANY" => Some("DE"),
        "FRANCE" => Some("FR"),
        "SPAIN" => Some("ES"),
        "ITALY" => Some("IT"),
        "IRELAND" => Some("IE"),
        "NETHERLANDS" => Some("NL"),
        "JAPAN" => Some("JP"),
        "CHINA" => Some("CN"),
        "NEW ZEALAND" => Some("NZ"),
        "SINGAPORE" => Some("SG"),
        "PHILIPPINES" => Some("PH"),
        "SOUTH AFRICA" => Some("ZA"),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if up.len() == 2 && up.bytes().all(|b| b.is_ascii_uppercase()) {
        return COUNTRY_ZONES.get_key_value(up.as_str()).map(|(k, _)| *k);
    }
    None
}

/// Countries that span several zones but where nearly everyone lives in one (over 90%).
const DOMINANT: &[(&str, &str)] = &[
    ("BR", "America/Sao_Paulo"),
    ("CL", "America/Santiago"),
    ("CN", "Asia/Shanghai"),
    ("EC", "America/Guayaquil"),
    ("ES", "Europe/Madrid"),
    ("MX", "America/Mexico_City"),
    ("NZ", "Pacific/Auckland"),
    ("PT", "Europe/Lisbon"),
    ("UA", "Europe/Kyiv"),
];

/// Old names kept by the tz database for compatibility (its "backward" file). They work, but a
/// country's zone should be reported by its current name.
const ALIASES: &[&str] = &[
    "Africa/Asmera", "Africa/Timbuktu", "America/Argentina/ComodRivadavia", "America/Atka", "America/Buenos_Aires", "America/Catamarca",
    "America/Coral_Harbour", "America/Cordoba", "America/Ensenada", "America/Fort_Wayne", "America/Godthab", "America/Indianapolis",
    "America/Jujuy", "America/Knox_IN", "America/Louisville", "America/Mendoza", "America/Montreal", "America/Porto_Acre", "America/Rosario",
    "America/Santa_Isabel", "America/Shiprock", "America/Virgin", "Antarctica/South_Pole", "Asia/Ashkhabad", "Asia/Calcutta",
    "Asia/Chongqing", "Asia/Chungking", "Asia/Dacca", "Asia/Harbin", "Asia/Istanbul", "Asia/Kashgar", "Asia/Katmandu", "Asia/Macao",
    "Asia/Rangoon", "Asia/Saigon", "Asia/Tel_Aviv", "Asia/Thimbu", "Asia/Ujung_Pandang", "Asia/Ulan_Bator", "Atlantic/Faeroe",
    "Atlantic/Jan_Mayen", "Australia/ACT", "Australia/Canberra", "Australia/LHI", "Australia/NSW", "Australia/North",
    "Australia/Queensland", "Australia/South", "Australia/Tasmania", "Australia/Victoria", "Australia/West", "Australia/Yancowinna",
    "Europe/Belfast", "Europe/Kiev", "Europe/Nicosia", "Europe/Tiraspol", "Europe/Uzhgorod", "Europe/Zaporozhye", "Pacific/Enderbury",
    "Pacific/Johnston", "Pacific/Ponape", "Pacific/Samoa", "Pacific/Truk", "Pacific/Yap",
];

/// Current names that are the smaller city of a country whose zones all keep the same clock.
const PREFER_NOT: &[&str] = &["Asia/Famagusta", "Asia/Samarkand", "Asia/Hebron"];

/// Countries whose every zone keeps the same clock (same offsets in winter and summer), mapped to
/// one of those zones. Built from the zone → country table, so it stays in step with it.
static COUNTRY_ZONES: Lazy<HashMap<&'static str, Option<Tz>>> = Lazy::new(|| {
    let probes = [Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap(), Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap()];
    let signature = |tz: &Tz| probes.map(|p| tz.offset_from_utc_datetime(&p.naive_utc()).fix().local_minus_utc());
    let mut by_country: HashMap<&'static str, Vec<Tz>> = HashMap::new();
    for tz in chrono_tz::TZ_VARIANTS.iter() {
        if let Some(c) = crate::tz_countries::country(tz.name()) {
            by_country.entry(c).or_default().push(*tz);
        }
    }
    by_country
        .into_iter()
        .map(|(c, zones)| {
            let first = signature(&zones[0]);
            let single = zones.iter().all(|z| signature(z) == first);
            // Report it by a current name, from the region most of the country's zones are in.
            let mut names: Vec<Tz> = zones.iter().copied().filter(|z| z.name().contains('/') && !ALIASES.contains(&z.name()) && !PREFER_NOT.contains(&z.name())).collect();
            if names.is_empty() {
                names = zones.clone();
            }
            let region = |z: &Tz| z.name().split('/').next().unwrap_or("").to_string();
            let mut counts: HashMap<String, usize> = HashMap::new();
            names.iter().for_each(|z| *counts.entry(region(z)).or_default() += 1);
            let main = counts.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0))).map(|(r, _)| r.clone()).unwrap_or_default();
            names.sort_by_key(|z| (region(z) != main, z.name()));
            (c, single.then_some(names[0]))
        })
        .collect()
});

/// The zone for a country when it has one (or one that nearly everyone uses).
fn country_zone(code: &str) -> Option<Tz> {
    if let Some((_, z)) = DOMINANT.iter().find(|(c, _)| *c == code) {
        return Tz::from_str(z).ok();
    }
    COUNTRY_ZONES.get(code).copied().flatten()
}

/// US states, Canadian provinces and Australian states, by code or name. A state split across zones
/// maps to the zone most of its people use.
fn region_zone(country: Option<&str>, raw: &str) -> Option<Tz> {
    let key = raw.trim().to_ascii_uppercase().replace('.', "");
    let find = |table: &[(&'static str, &'static str, &'static str)]| table.iter().find(|(code, name, _)| *code == key || name.eq_ignore_ascii_case(&key)).map(|(_, _, z)| *z);
    let zone = match country {
        Some("US") => find(US_STATES),
        Some("CA") => find(CA_PROVINCES),
        Some("AU") => find(AU_STATES),
        // Without a country, US and Canadian codes don't overlap; Australian ones do (WA, NT), so
        // they need the country.
        None => find(US_STATES).or_else(|| find(CA_PROVINCES)),
        Some(_) => None,
    }?;
    Tz::from_str(zone).ok()
}

const US_STATES: &[(&str, &str, &str)] = &[
    ("AL", "Alabama", "America/Chicago"),
    ("AK", "Alaska", "America/Anchorage"),
    ("AZ", "Arizona", "America/Phoenix"),
    ("AR", "Arkansas", "America/Chicago"),
    ("CA", "California", "America/Los_Angeles"),
    ("CO", "Colorado", "America/Denver"),
    ("CT", "Connecticut", "America/New_York"),
    ("DE", "Delaware", "America/New_York"),
    ("DC", "District of Columbia", "America/New_York"),
    ("FL", "Florida", "America/New_York"),
    ("GA", "Georgia", "America/New_York"),
    ("HI", "Hawaii", "Pacific/Honolulu"),
    ("ID", "Idaho", "America/Boise"),
    ("IL", "Illinois", "America/Chicago"),
    ("IN", "Indiana", "America/Indiana/Indianapolis"),
    ("IA", "Iowa", "America/Chicago"),
    ("KS", "Kansas", "America/Chicago"),
    ("KY", "Kentucky", "America/New_York"),
    ("LA", "Louisiana", "America/Chicago"),
    ("ME", "Maine", "America/New_York"),
    ("MD", "Maryland", "America/New_York"),
    ("MA", "Massachusetts", "America/New_York"),
    ("MI", "Michigan", "America/Detroit"),
    ("MN", "Minnesota", "America/Chicago"),
    ("MS", "Mississippi", "America/Chicago"),
    ("MO", "Missouri", "America/Chicago"),
    ("MT", "Montana", "America/Denver"),
    ("NE", "Nebraska", "America/Chicago"),
    ("NV", "Nevada", "America/Los_Angeles"),
    ("NH", "New Hampshire", "America/New_York"),
    ("NJ", "New Jersey", "America/New_York"),
    ("NM", "New Mexico", "America/Denver"),
    ("NY", "New York", "America/New_York"),
    ("NC", "North Carolina", "America/New_York"),
    ("ND", "North Dakota", "America/Chicago"),
    ("OH", "Ohio", "America/New_York"),
    ("OK", "Oklahoma", "America/Chicago"),
    ("OR", "Oregon", "America/Los_Angeles"),
    ("PA", "Pennsylvania", "America/New_York"),
    ("RI", "Rhode Island", "America/New_York"),
    ("SC", "South Carolina", "America/New_York"),
    ("SD", "South Dakota", "America/Chicago"),
    ("TN", "Tennessee", "America/Chicago"),
    ("TX", "Texas", "America/Chicago"),
    ("UT", "Utah", "America/Denver"),
    ("VT", "Vermont", "America/New_York"),
    ("VA", "Virginia", "America/New_York"),
    ("WA", "Washington", "America/Los_Angeles"),
    ("WV", "West Virginia", "America/New_York"),
    ("WI", "Wisconsin", "America/Chicago"),
    ("WY", "Wyoming", "America/Denver"),
    ("PR", "Puerto Rico", "America/Puerto_Rico"),
    ("GU", "Guam", "Pacific/Guam"),
    ("VI", "US Virgin Islands", "America/St_Thomas"),
];

const CA_PROVINCES: &[(&str, &str, &str)] = &[
    ("AB", "Alberta", "America/Edmonton"),
    ("BC", "British Columbia", "America/Vancouver"),
    ("MB", "Manitoba", "America/Winnipeg"),
    ("NB", "New Brunswick", "America/Moncton"),
    ("NL", "Newfoundland and Labrador", "America/St_Johns"),
    ("NS", "Nova Scotia", "America/Halifax"),
    ("NT", "Northwest Territories", "America/Edmonton"),
    ("NU", "Nunavut", "America/Toronto"),
    ("ON", "Ontario", "America/Toronto"),
    ("PE", "Prince Edward Island", "America/Halifax"),
    ("QC", "Quebec", "America/Toronto"),
    ("SK", "Saskatchewan", "America/Regina"),
    ("YT", "Yukon", "America/Whitehorse"),
];

const AU_STATES: &[(&str, &str, &str)] = &[
    ("ACT", "Australian Capital Territory", "Australia/Sydney"),
    ("NSW", "New South Wales", "Australia/Sydney"),
    ("NT", "Northern Territory", "Australia/Darwin"),
    ("QLD", "Queensland", "Australia/Brisbane"),
    ("SA", "South Australia", "Australia/Adelaide"),
    ("TAS", "Tasmania", "Australia/Hobart"),
    ("VIC", "Victoria", "Australia/Melbourne"),
    ("WA", "Western Australia", "Australia/Perth"),
];

/// A US ZIP code's zone, from its first three digits (the USPS sectional center), where the
/// sectional center sits in one zone.
fn us_zip_zone(raw: &str) -> Option<Tz> {
    let digits: String = raw.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() != 5 {
        return None;
    }
    let p: u16 = digits[..3].parse().ok()?;
    // Sectional centers on the other side of a zone line from the rest of their state.
    let special = match p {
        324..=325 => Some("America/Chicago"),     // Florida panhandle (Panama City, Pensacola)
        373..=379 => Some("America/New_York"),    // East Tennessee
        420..=424 => Some("America/Chicago"),     // Western Kentucky
        463..=464 | 476..=477 => Some("America/Chicago"), // Northwest and southwest Indiana
        577 => Some("America/Denver"),            // Western South Dakota
        798..=799 | 885 => Some("America/Denver"), // El Paso
        835 | 838 => Some("America/Los_Angeles"), // Idaho panhandle
        _ => None,
    };
    if let Some(z) = special {
        return Tz::from_str(z).ok();
    }
    let state = ZIP3.iter().find(|(lo, hi, _)| (*lo..=*hi).contains(&p)).map(|(_, _, s)| *s)?;
    region_zone(Some("US"), state)
}

/// USPS three-digit ZIP prefixes by state.
const ZIP3: &[(u16, u16, &str)] = &[
    (5, 5, "NY"),
    (6, 9, "PR"),
    (10, 27, "MA"),
    (28, 29, "RI"),
    (30, 38, "NH"),
    (39, 49, "ME"),
    (50, 54, "VT"),
    (55, 55, "MA"),
    (56, 59, "VT"),
    (60, 69, "CT"),
    (70, 89, "NJ"),
    (100, 149, "NY"),
    (150, 196, "PA"),
    (197, 199, "DE"),
    (200, 205, "DC"),
    (206, 219, "MD"),
    (220, 246, "VA"),
    (247, 268, "WV"),
    (270, 289, "NC"),
    (290, 299, "SC"),
    (300, 319, "GA"),
    (320, 349, "FL"),
    (350, 369, "AL"),
    (370, 385, "TN"),
    (386, 397, "MS"),
    (398, 399, "GA"),
    (400, 427, "KY"),
    (430, 459, "OH"),
    (460, 479, "IN"),
    (480, 499, "MI"),
    (500, 528, "IA"),
    (530, 549, "WI"),
    (550, 567, "MN"),
    (569, 569, "DC"),
    (570, 577, "SD"),
    (580, 588, "ND"),
    (590, 599, "MT"),
    (600, 629, "IL"),
    (630, 658, "MO"),
    (660, 679, "KS"),
    (680, 693, "NE"),
    (700, 714, "LA"),
    (716, 729, "AR"),
    (730, 749, "OK"),
    (750, 799, "TX"),
    (800, 816, "CO"),
    (820, 831, "WY"),
    (832, 838, "ID"),
    (840, 847, "UT"),
    (850, 865, "AZ"),
    (870, 884, "NM"),
    (889, 898, "NV"),
    (900, 961, "CA"),
    (967, 968, "HI"),
    (970, 979, "OR"),
    (980, 994, "WA"),
    (995, 999, "AK"),
];

/// A phone number's zone: the country calling code when that country has one zone, or in North
/// America the area code (where the area code sits in one zone).
pub fn phone_zone(raw: &str) -> Option<Tz> {
    let e164 = crate::models::normalize_phone(raw)?;
    let digits = &e164[1..];
    if let Some(rest) = digits.strip_prefix('1') {
        let area = rest.get(..3)?;
        let zone = NANP.iter().find(|(_, codes)| codes.split(' ').any(|c| c == area)).map(|(z, _)| *z)?;
        return Tz::from_str(zone).ok();
    }
    // Longest calling code first (codes are 1-3 digits and prefix-free).
    let country = (1..=3).rev().find_map(|n| digits.get(..n).and_then(|p| CALLING_CODES.iter().find(|(c, _)| *c == p)).map(|(_, iso)| *iso))?;
    country_zone(country)
}

/// North American area codes by zone. Area codes that straddle a zone line are left out.
const NANP: &[(&str, &str)] = &[
    (
        "America/New_York",
        "203 475 860 959 302 202 771 239 305 321 352 386 407 561 656 689 727 754 772 786 813 863 904 941 954 \
         229 404 470 478 678 706 762 770 912 943 260 317 463 574 765 502 606 859 207 227 240 301 410 443 667 \
         339 351 413 508 617 774 781 857 978 603 201 551 609 640 732 848 856 862 908 973 \
         212 315 332 347 516 518 585 607 631 646 680 716 718 838 845 914 917 929 934 \
         252 336 472 704 743 828 910 919 980 984 216 220 234 326 330 380 419 436 440 513 567 614 740 937 \
         215 223 267 272 412 445 484 570 582 610 717 724 814 835 878 401 803 839 843 854 864 423 865 802 \
         276 434 540 571 703 757 804 826 948 304 681",
    ),
    ("America/Detroit", "231 248 269 313 517 586 616 679 734 810 906 947 989"),
    (
        "America/Chicago",
        "205 251 256 334 659 938 327 479 501 870 217 224 309 312 331 447 464 618 630 708 730 773 779 815 847 872 219 \
         319 515 563 641 712 316 620 785 913 270 364 225 318 337 504 985 218 320 507 612 651 763 952 \
         228 601 662 769 314 417 557 573 636 660 816 402 531 701 405 539 572 580 918 605 615 629 731 901 931 \
         210 214 254 281 325 346 361 409 430 432 469 512 682 713 726 737 806 817 830 832 903 936 940 945 956 972 979 \
         262 274 353 414 534 608 715 920",
    ),
    ("America/Denver", "303 719 720 970 983 406 505 575 385 435 801 307 915"),
    ("America/Boise", "208 986"),
    ("America/Phoenix", "480 520 602 623 928"),
    (
        "America/Los_Angeles",
        "209 213 279 310 323 341 350 369 408 415 424 442 510 530 559 562 619 626 628 650 657 661 669 707 714 747 760 \
         805 818 820 831 840 858 909 916 925 949 951 702 725 775 458 503 541 971 206 253 360 425 509 564",
    ),
    ("America/Anchorage", "907"),
    ("Pacific/Honolulu", "808"),
    (
        "America/Toronto",
        "226 249 289 343 365 382 416 437 519 548 613 647 683 705 742 753 905 263 354 367 418 438 450 468 514 579 581 819 873",
    ),
    ("America/Halifax", "782 902"),
    ("America/Moncton", "428 506"),
    ("America/St_Johns", "709"),
    ("America/Winnipeg", "204 431 584"),
    ("America/Regina", "306 474 639"),
    ("America/Edmonton", "368 403 587 780 825"),
    ("America/Vancouver", "236 250 257 604 672 778"),
    ("America/Puerto_Rico", "787 939"),
    ("America/St_Thomas", "340"),
    ("America/Santo_Domingo", "809 829 849"),
    ("America/Jamaica", "658 876"),
    ("America/Port_of_Spain", "868"),
    ("America/Nassau", "242"),
    ("America/Barbados", "246"),
    ("Atlantic/Bermuda", "441"),
    ("America/Cayman", "345"),
    ("Pacific/Guam", "671"),
    ("Pacific/Saipan", "670"),
    ("Pacific/Pago_Pago", "684"),
];

/// International calling codes (outside North America) and their countries.
const CALLING_CODES: &[(&str, &str)] = &[
    ("20", "EG"), ("211", "SS"), ("212", "MA"), ("213", "DZ"), ("216", "TN"), ("218", "LY"), ("220", "GM"), ("221", "SN"),
    ("225", "CI"), ("233", "GH"), ("234", "NG"), ("237", "CM"), ("250", "RW"), ("251", "ET"), ("254", "KE"), ("255", "TZ"),
    ("256", "UG"), ("260", "ZM"), ("263", "ZW"), ("27", "ZA"), ("30", "GR"), ("31", "NL"), ("32", "BE"), ("33", "FR"),
    ("34", "ES"), ("351", "PT"), ("352", "LU"), ("353", "IE"), ("354", "IS"), ("355", "AL"), ("356", "MT"), ("357", "CY"),
    ("358", "FI"), ("359", "BG"), ("36", "HU"), ("370", "LT"), ("371", "LV"), ("372", "EE"), ("373", "MD"), ("374", "AM"),
    ("375", "BY"), ("380", "UA"), ("381", "RS"), ("382", "ME"), ("385", "HR"), ("386", "SI"), ("387", "BA"), ("389", "MK"),
    ("39", "IT"), ("40", "RO"), ("41", "CH"), ("420", "CZ"), ("421", "SK"), ("423", "LI"), ("43", "AT"), ("44", "GB"),
    ("45", "DK"), ("46", "SE"), ("47", "NO"), ("48", "PL"), ("49", "DE"), ("51", "PE"), ("52", "MX"), ("53", "CU"),
    ("54", "AR"), ("55", "BR"), ("56", "CL"), ("57", "CO"), ("58", "VE"), ("501", "BZ"), ("502", "GT"), ("503", "SV"),
    ("504", "HN"), ("505", "NI"), ("506", "CR"), ("507", "PA"), ("591", "BO"), ("593", "EC"), ("595", "PY"), ("598", "UY"),
    ("60", "MY"), ("63", "PH"), ("64", "NZ"), ("65", "SG"), ("66", "TH"), ("81", "JP"), ("82", "KR"), ("84", "VN"),
    ("852", "HK"), ("853", "MO"), ("855", "KH"), ("856", "LA"), ("86", "CN"), ("880", "BD"), ("886", "TW"), ("90", "TR"),
    ("91", "IN"), ("92", "PK"), ("93", "AF"), ("94", "LK"), ("95", "MM"), ("960", "MV"), ("961", "LB"), ("962", "JO"),
    ("963", "SY"), ("964", "IQ"), ("965", "KW"), ("966", "SA"), ("967", "YE"), ("968", "OM"), ("970", "PS"), ("971", "AE"),
    ("972", "IL"), ("973", "BH"), ("974", "QA"), ("975", "BT"), ("977", "NP"), ("98", "IR"), ("992", "TJ"), ("993", "TM"),
    ("994", "AZ"), ("995", "GE"), ("996", "KG"), ("998", "UZ"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LA: Tz = chrono_tz::America::Los_Angeles;

    fn zone(props: Value, phone: Option<&str>) -> (&'static str, &'static str) {
        let (tz, src) = resolve_parts(&props, phone, LA);
        (tz.name(), src.as_str())
    }

    #[test]
    fn profile_zone_wins_and_must_be_real() {
        assert_eq!(zone(json!({"timezone": "Asia/Kathmandu", "country": "US"}), Some("+18085550100")), ("Asia/Kathmandu", "profile"));
        assert_eq!(zone(json!({"timezone": "Asia/Calcutta"}), None).1, "profile");
        assert_eq!(zone(json!({"timezone": "Mars/Olympus"}), None), ("America/Los_Angeles", "account"));
        assert_eq!(zone(json!({"timezone": "EST"}), None), ("America/Los_Angeles", "account"), "abbreviations aren't zones");
        assert_eq!(zone(json!({"timezone": 5}), None).1, "account");
    }

    #[test]
    fn regions_zips_and_countries() {
        assert_eq!(zone(json!({"country": "US", "region": "HI"}), None), ("Pacific/Honolulu", "region"));
        assert_eq!(zone(json!({"region": "New York"}), None), ("America/New_York", "region"));
        assert_eq!(zone(json!({"region": "NL"}), None), ("America/St_Johns", "region"));
        assert_eq!(zone(json!({"country": "AU", "region": "SA"}), None), ("Australia/Adelaide", "region"));
        assert_eq!(zone(json!({"region": "WA", "country": "Australia"}), None).0, "Australia/Perth");
        assert_eq!(zone(json!({"country": "US", "zip": "96813"}), None), ("Pacific/Honolulu", "zip"));
        assert_eq!(zone(json!({"country": "USA", "zip": "79901-1234"}), None), ("America/Denver", "zip"));
        assert_eq!(zone(json!({"country": "US", "zip": "10001"}), None).0, "America/New_York");
        assert_eq!(zone(json!({"zip": "10001"}), None).1, "account", "a zip without a country could be anywhere");
        assert_eq!(zone(json!({"country": "IN"}), None), ("Asia/Kolkata", "country"));
        assert_eq!(zone(json!({"country": "Nepal"}), None), ("Asia/Kathmandu", "country"));
        assert_eq!(zone(json!({"country": "de"}), None).0, "Europe/Berlin");
        assert_eq!(zone(json!({"country": "ES"}), None).0, "Europe/Madrid");
        // Several zones and no other clue: the account's zone.
        assert_eq!(zone(json!({"country": "US"}), None), ("America/Los_Angeles", "account"));
        assert_eq!(zone(json!({"country": "RU"}), None).1, "account");
    }

    #[test]
    fn phone_numbers() {
        assert_eq!(zone(json!({}), Some("+18085550100")), ("Pacific/Honolulu", "phone"));
        assert_eq!(zone(json!({}), Some("+12125550100")).0, "America/New_York");
        assert_eq!(zone(json!({}), Some("(503) 555-0100")).0, "America/Los_Angeles");
        assert_eq!(zone(json!({}), Some("+14165550100")).0, "America/Toronto");
        assert_eq!(zone(json!({}), Some("+919812345678")).0, "Asia/Kolkata");
        assert_eq!(zone(json!({}), Some("+9779812345678")).0, "Asia/Kathmandu");
        assert_eq!(zone(json!({}), Some("+442079460958")).0, "Europe/London");
        // Straddling area code, multi-zone country, unknown: no guess.
        assert_eq!(zone(json!({}), Some("+18505550100")).1, "account");
        assert_eq!(zone(json!({}), Some("+61412345678")).1, "account");
        assert_eq!(zone(json!({}), Some("+74951234567")).1, "account");
        // The country beats the phone; a region in the US beats both.
        assert_eq!(zone(json!({"country": "GB"}), Some("+18085550100")).0, "Europe/London");
        assert_eq!(zone(json!({"country": "US"}), Some("+18085550100")).0, "Pacific/Honolulu");
    }
}
