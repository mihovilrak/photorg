# Third-party notices

## photorg

Copyright (c) Mihovil Rak. Licensed under the MIT License; see `LICENSE`.

## GeoNames `cities1000`

The offline reverse geocoder embeds a compiled subset of the GeoNames
`cities1000` dataset (`data/cities.bin`): place name, admin-1 region name,
country code, latitude and longitude for populated places over 1,000 people.

    Geocoding data (c) GeoNames, licensed under CC BY 4.0
    https://www.geonames.org/
    https://creativecommons.org/licenses/by/4.0/

The data is redistributed unmodified in substance; `data/make_cities.py`
only re-encodes it (unused columns dropped, region names interned,
coordinates narrowed to f32, records reordered into k-d tree order).

Builds made with `--no-default-features` contain no GeoNames data.

## Rust dependencies

Build- and link-time dependencies are listed in `Cargo.toml` and pinned in
`Cargo.lock`. They are MIT, Apache-2.0, or dual MIT/Apache-2.0 licensed. Run
`cargo tree` for the full graph.
