//! Offline reverse geocoding.
//!
//! Embedded GeoNames `cities1000` + k-d tree nearest neighbour: microseconds
//! per query, deterministic, no network and no rate limits. The whole feature
//! compiles out when the `geocoding` feature is off, and the rest of the crate
//! never learns about it — a `Locator` simply resolves nothing.

use std::collections::HashMap;

use crate::config::LocationDepth;
use crate::template::Location;

/// Was the binary built with the offline dataset?
pub const AVAILABLE: bool = cfg!(feature = "geocoding");

/// Cache granularity: 3 decimals is ~100 m, so a burst of shots from one spot
/// shares a single k-d tree query.
const CACHE_SCALE: f32 = 1000.0;

pub struct Locator {
    #[allow(dead_code)]
    depth: LocationDepth,
    cache: HashMap<(i32, i32), Option<Location>>,
}

impl Locator {
    /// `None` when the binary was built without the dataset.
    pub fn new(depth: LocationDepth) -> Option<Locator> {
        if !AVAILABLE {
            return None;
        }
        Some(Locator {
            depth,
            cache: HashMap::new(),
        })
    }

    pub fn lookup(&mut self, lat: f32, lon: f32) -> Option<Location> {
        let key = (
            (lat * CACHE_SCALE).round() as i32,
            (lon * CACHE_SCALE).round() as i32,
        );
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        let found = self.resolve(lat, lon);
        self.cache.insert(key, found.clone());
        found
    }

    pub fn cached_queries(&self) -> usize {
        self.cache.len()
    }

    #[cfg(feature = "geocoding")]
    fn resolve(&self, lat: f32, lon: f32) -> Option<Location> {
        let hit = crate::cities::nearest(lat, lon)?;
        let country = country_name(hit.cc);
        let region = non_empty(hit.region);
        // Nearest populated place: country and region are reliable, the "city"
        // may be a town 30 km away (documented, not hidden).
        let city = non_empty(hit.name);
        Some(match self.depth {
            LocationDepth::Country => Location {
                country,
                region: None,
                city: None,
            },
            LocationDepth::Region => Location {
                country,
                region,
                city: None,
            },
            LocationDepth::City => Location {
                country,
                region,
                city,
            },
        })
    }

    #[cfg(not(feature = "geocoding"))]
    fn resolve(&self, _lat: f32, _lon: f32) -> Option<Location> {
        None
    }
}

#[cfg(feature = "geocoding")]
fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// ISO 3166-1 alpha-2 to country name. GeoNames stores only the code, and
/// `2026/08/HR` is not what anyone asked for. Unknown codes pass through.
#[cfg(feature = "geocoding")]
fn country_name(cc: &str) -> Option<String> {
    let cc = cc.trim();
    if cc.is_empty() {
        return None;
    }
    let upper = cc.to_ascii_uppercase();
    Some(
        COUNTRIES
            .binary_search_by(|(code, _)| (*code).cmp(upper.as_str()))
            .map(|i| COUNTRIES[i].1.to_string())
            .unwrap_or(upper),
    )
}

#[cfg(feature = "geocoding")]
#[rustfmt::skip]
static COUNTRIES: &[(&str, &str)] = &[
    ("AD", "Andorra"), ("AE", "United Arab Emirates"), ("AF", "Afghanistan"),
    ("AG", "Antigua and Barbuda"), ("AI", "Anguilla"), ("AL", "Albania"),
    ("AM", "Armenia"), ("AO", "Angola"), ("AQ", "Antarctica"), ("AR", "Argentina"),
    ("AS", "American Samoa"), ("AT", "Austria"), ("AU", "Australia"), ("AW", "Aruba"),
    ("AX", "Aland Islands"), ("AZ", "Azerbaijan"), ("BA", "Bosnia and Herzegovina"),
    ("BB", "Barbados"), ("BD", "Bangladesh"), ("BE", "Belgium"), ("BF", "Burkina Faso"),
    ("BG", "Bulgaria"), ("BH", "Bahrain"), ("BI", "Burundi"), ("BJ", "Benin"),
    ("BL", "Saint Barthelemy"), ("BM", "Bermuda"), ("BN", "Brunei"), ("BO", "Bolivia"),
    ("BQ", "Caribbean Netherlands"), ("BR", "Brazil"), ("BS", "Bahamas"), ("BT", "Bhutan"),
    ("BV", "Bouvet Island"), ("BW", "Botswana"), ("BY", "Belarus"), ("BZ", "Belize"),
    ("CA", "Canada"), ("CC", "Cocos Islands"), ("CD", "DR Congo"),
    ("CF", "Central African Republic"), ("CG", "Congo"), ("CH", "Switzerland"),
    ("CI", "Ivory Coast"), ("CK", "Cook Islands"), ("CL", "Chile"), ("CM", "Cameroon"),
    ("CN", "China"), ("CO", "Colombia"), ("CR", "Costa Rica"), ("CU", "Cuba"),
    ("CV", "Cape Verde"), ("CW", "Curacao"), ("CX", "Christmas Island"), ("CY", "Cyprus"),
    ("CZ", "Czechia"), ("DE", "Germany"), ("DJ", "Djibouti"), ("DK", "Denmark"),
    ("DM", "Dominica"), ("DO", "Dominican Republic"), ("DZ", "Algeria"), ("EC", "Ecuador"),
    ("EE", "Estonia"), ("EG", "Egypt"), ("EH", "Western Sahara"), ("ER", "Eritrea"),
    ("ES", "Spain"), ("ET", "Ethiopia"), ("FI", "Finland"), ("FJ", "Fiji"),
    ("FK", "Falkland Islands"), ("FM", "Micronesia"), ("FO", "Faroe Islands"),
    ("FR", "France"), ("GA", "Gabon"), ("GB", "United Kingdom"), ("GD", "Grenada"),
    ("GE", "Georgia"), ("GF", "French Guiana"), ("GG", "Guernsey"), ("GH", "Ghana"),
    ("GI", "Gibraltar"), ("GL", "Greenland"), ("GM", "Gambia"), ("GN", "Guinea"),
    ("GP", "Guadeloupe"), ("GQ", "Equatorial Guinea"), ("GR", "Greece"),
    ("GS", "South Georgia and the South Sandwich Islands"), ("GT", "Guatemala"),
    ("GU", "Guam"), ("GW", "Guinea-Bissau"), ("GY", "Guyana"), ("HK", "Hong Kong"),
    ("HM", "Heard Island and McDonald Islands"), ("HN", "Honduras"), ("HR", "Croatia"),
    ("HT", "Haiti"), ("HU", "Hungary"), ("ID", "Indonesia"), ("IE", "Ireland"),
    ("IL", "Israel"), ("IM", "Isle of Man"), ("IN", "India"),
    ("IO", "British Indian Ocean Territory"), ("IQ", "Iraq"), ("IR", "Iran"),
    ("IS", "Iceland"), ("IT", "Italy"), ("JE", "Jersey"), ("JM", "Jamaica"),
    ("JO", "Jordan"), ("JP", "Japan"), ("KE", "Kenya"), ("KG", "Kyrgyzstan"),
    ("KH", "Cambodia"), ("KI", "Kiribati"), ("KM", "Comoros"),
    ("KN", "Saint Kitts and Nevis"), ("KP", "North Korea"), ("KR", "South Korea"),
    ("KW", "Kuwait"), ("KY", "Cayman Islands"), ("KZ", "Kazakhstan"), ("LA", "Laos"),
    ("LB", "Lebanon"), ("LC", "Saint Lucia"), ("LI", "Liechtenstein"), ("LK", "Sri Lanka"),
    ("LR", "Liberia"), ("LS", "Lesotho"), ("LT", "Lithuania"), ("LU", "Luxembourg"),
    ("LV", "Latvia"), ("LY", "Libya"), ("MA", "Morocco"), ("MC", "Monaco"),
    ("MD", "Moldova"), ("ME", "Montenegro"), ("MF", "Saint Martin"), ("MG", "Madagascar"),
    ("MH", "Marshall Islands"), ("MK", "North Macedonia"), ("ML", "Mali"), ("MM", "Myanmar"),
    ("MN", "Mongolia"), ("MO", "Macao"), ("MP", "Northern Mariana Islands"),
    ("MQ", "Martinique"), ("MR", "Mauritania"), ("MS", "Montserrat"), ("MT", "Malta"),
    ("MU", "Mauritius"), ("MV", "Maldives"), ("MW", "Malawi"), ("MX", "Mexico"),
    ("MY", "Malaysia"), ("MZ", "Mozambique"), ("NA", "Namibia"), ("NC", "New Caledonia"),
    ("NE", "Niger"), ("NF", "Norfolk Island"), ("NG", "Nigeria"), ("NI", "Nicaragua"),
    ("NL", "Netherlands"), ("NO", "Norway"), ("NP", "Nepal"), ("NR", "Nauru"),
    ("NU", "Niue"), ("NZ", "New Zealand"), ("OM", "Oman"), ("PA", "Panama"),
    ("PE", "Peru"), ("PF", "French Polynesia"), ("PG", "Papua New Guinea"),
    ("PH", "Philippines"), ("PK", "Pakistan"), ("PL", "Poland"),
    ("PM", "Saint Pierre and Miquelon"), ("PN", "Pitcairn Islands"), ("PR", "Puerto Rico"),
    ("PS", "Palestine"), ("PT", "Portugal"), ("PW", "Palau"), ("PY", "Paraguay"),
    ("QA", "Qatar"), ("RE", "Reunion"), ("RO", "Romania"), ("RS", "Serbia"),
    ("RU", "Russia"), ("RW", "Rwanda"), ("SA", "Saudi Arabia"), ("SB", "Solomon Islands"),
    ("SC", "Seychelles"), ("SD", "Sudan"), ("SE", "Sweden"), ("SG", "Singapore"),
    ("SH", "Saint Helena"), ("SI", "Slovenia"), ("SJ", "Svalbard and Jan Mayen"),
    ("SK", "Slovakia"), ("SL", "Sierra Leone"), ("SM", "San Marino"), ("SN", "Senegal"),
    ("SO", "Somalia"), ("SR", "Suriname"), ("SS", "South Sudan"),
    ("ST", "Sao Tome and Principe"), ("SV", "El Salvador"), ("SX", "Sint Maarten"),
    ("SY", "Syria"), ("SZ", "Eswatini"), ("TC", "Turks and Caicos Islands"), ("TD", "Chad"),
    ("TF", "French Southern Territories"), ("TG", "Togo"), ("TH", "Thailand"),
    ("TJ", "Tajikistan"), ("TK", "Tokelau"), ("TL", "Timor-Leste"), ("TM", "Turkmenistan"),
    ("TN", "Tunisia"), ("TO", "Tonga"), ("TR", "Turkiye"), ("TT", "Trinidad and Tobago"),
    ("TV", "Tuvalu"), ("TW", "Taiwan"), ("TZ", "Tanzania"), ("UA", "Ukraine"),
    ("UG", "Uganda"), ("UM", "United States Minor Outlying Islands"), ("US", "United States"),
    ("UY", "Uruguay"), ("UZ", "Uzbekistan"), ("VA", "Vatican City"),
    ("VC", "Saint Vincent and the Grenadines"), ("VE", "Venezuela"),
    ("VG", "British Virgin Islands"), ("VI", "U.S. Virgin Islands"), ("VN", "Vietnam"),
    ("VU", "Vanuatu"), ("WF", "Wallis and Futuna"), ("WS", "Samoa"), ("XK", "Kosovo"),
    ("YE", "Yemen"), ("YT", "Mayotte"), ("ZA", "South Africa"), ("ZM", "Zambia"),
    ("ZW", "Zimbabwe"),
];

#[cfg(all(test, feature = "geocoding"))]
mod tests {
    use super::*;

    #[test]
    fn country_table_is_sorted_for_binary_search() {
        assert!(COUNTRIES.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn maps_codes_to_names_and_passes_unknowns_through() {
        assert_eq!(country_name("hr").as_deref(), Some("Croatia"));
        assert_eq!(country_name("ZZ").as_deref(), Some("ZZ"));
        assert_eq!(country_name("  "), None);
    }

    #[test]
    fn resolves_zagreb_to_croatia() {
        let mut loc = Locator::new(LocationDepth::Region).unwrap();
        let found = loc.lookup(45.815, 15.982).unwrap();
        assert_eq!(found.country.as_deref(), Some("Croatia"));
        assert!(found.region.is_some());
        assert_eq!(found.city, None);
    }

    #[test]
    fn nearby_coordinates_hit_the_cache() {
        let mut loc = Locator::new(LocationDepth::City).unwrap();
        loc.lookup(45.8150, 15.9820);
        loc.lookup(45.81501, 15.98201);
        assert_eq!(loc.cached_queries(), 1);
    }

    #[test]
    fn hemispheres_are_not_mirrored() {
        let mut loc = Locator::new(LocationDepth::Country).unwrap();
        assert_eq!(
            loc.lookup(-33.87, 151.21).unwrap().country.as_deref(),
            Some("Australia")
        );
        assert_eq!(
            loc.lookup(40.71, -74.0).unwrap().country.as_deref(),
            Some("United States")
        );
    }
}
