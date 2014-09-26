use std::collections::HashMap;
use std::sync::Arc;
use serialize::json;
use serialize::hex::ToHex;
use time::Timespec;

use conduit::{Request, Response};
use conduit_router::RequestParams;
use conduit_json_parser;
use pg::{PostgresRow, PostgresStatement};
use pg::types::ToSql;
use curl::http;

use app::{App, RequestApp};
use db::{Connection, RequestTransaction};
use dependency::Dependency;
use git;
use user::{RequestUser, User};
use util::{RequestUtils, CargoResult, Require, internal, ChainError, human};
use util::{LimitErrorReader, HashingReader};
use util::errors::{NotFound, CargoError};
use version::{Version, EncodableVersion};

#[deriving(Clone)]
pub struct Package {
    pub id: i32,
    pub name: String,
    pub user_id: i32,
    pub updated_at: Timespec,
    pub created_at: Timespec,
    pub downloads: i32,
}

#[deriving(Encodable, Decodable)]
pub struct EncodablePackage {
    pub id: String,
    pub name: String,
    pub versions: Vec<i32>,
    pub updated_at: String,
    pub created_at: String,
    pub downloads: i32,
}

impl Package {
    pub fn from_row(row: &PostgresRow) -> Package {
        Package {
            id: row.get("id"),
            name: row.get("name"),
            user_id: row.get("user_id"),
            updated_at: row.get("updated_at"),
            created_at: row.get("created_at"),
            downloads: row.get("downloads"),
        }
    }

    pub fn find(conn: &Connection, id: i32) -> CargoResult<Package> {
        let stmt = try!(conn.prepare("SELECT * FROM packages \
                                      WHERE id = $1"));
        match try!(stmt.query(&[&id])).next() {
            Some(row) => Ok(Package::from_row(&row)),
            None => Err(NotFound.box_error()),
        }
    }

    pub fn find_by_name(conn: &Connection, name: &str) -> CargoResult<Package> {
        let stmt = try!(conn.prepare("SELECT * FROM packages \
                                      WHERE name = $1 LIMIT 1"));
        match try!(stmt.query(&[&name as &ToSql])).next() {
            Some(row) => Ok(Package::from_row(&row)),
            None => Err(NotFound.box_error()),
        }
    }

    pub fn find_or_insert(conn: &Connection, name: &str,
                          user_id: i32) -> CargoResult<Package> {
        // TODO: like with users, this is sadly racy

        let stmt = try!(conn.prepare("SELECT * FROM packages WHERE name = $1"));
        let mut rows = try!(stmt.query(&[&name as &ToSql]));
        match rows.next() {
            Some(row) => return Ok(Package::from_row(&row)),
            None => {}
        }
        let stmt = try!(conn.prepare("INSERT INTO packages \
                                      (name, user_id, created_at,
                                       updated_at, downloads) \
                                      VALUES ($1, $2, $3, $4, 0) \
                                      RETURNING *"));
        let now = ::now();
        let mut rows = try!(stmt.query(&[&name as &ToSql, &user_id, &now, &now]));
        Ok(Package::from_row(&try!(rows.next().require(|| {
            internal("no package returned")
        }))))
    }

    pub fn valid_name(name: &str) -> bool {
        if name.len() == 0 { return false }
        name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    }

    fn encodable(self, versions: Vec<i32>) -> EncodablePackage {
        let Package { name, created_at, updated_at, downloads, .. } = self;
        EncodablePackage {
            id: name.clone(),
            name: name,
            versions: versions,
            updated_at: ::encode_time(updated_at),
            created_at: ::encode_time(created_at),
            downloads: downloads,
        }
    }

    pub fn versions(&self, conn: &Connection) -> CargoResult<Vec<Version>> {
        let stmt = try!(conn.prepare("SELECT * FROM versions \
                                      WHERE package_id = $1"));
        let rows = try!(stmt.query(&[&self.id]));
        Ok(rows.map(|r| Version::from_row(&r)).collect())
    }

    pub fn dl_path(&self, version: &str) -> String {
        format!("/download/{}/{}-{}.tar.gz", self.name, self.name, version)
    }

    pub fn s3_path(&self, version: &str) -> String {
        format!("/pkg/{}/{}-{}.tar.gz", self.name, self.name, version)
    }

    pub fn encode_many(conn: &Connection, pkgs: Vec<Package>)
                       -> CargoResult<Vec<EncodablePackage>> {
        // TODO: can rust-postgres do this escaping?
        let pkgids: Vec<i32> = pkgs.iter().map(|p| p.id).collect();
        let mut map = HashMap::new();
        let query = format!("'{{{:#}}}'::int[]", pkgids.as_slice());
        let stmt = try!(conn.prepare(format!("SELECT id, package_id FROM versions \
                                              WHERE package_id = ANY({})",
                                             query).as_slice()));
        for row in try!(stmt.query(&[])) {
            map.find_or_insert(row.get("package_id"), Vec::new())
               .push(row.get("id"));
        }

        Ok(pkgs.into_iter().map(|p| {
            let id = p.id;
            p.encodable(map.pop(&id).unwrap())
        }).collect())
    }
}

pub fn index(req: &mut Request) -> CargoResult<Response> {
    let conn = try!(req.tx());
    let query = req.query();
    let page = query.find_equiv(&"page").map(|s| s.as_slice())
                    .and_then(from_str::<i64>).unwrap_or(1);
    let limit = query.find_equiv(&"per_page").map(|s| s.as_slice())
                     .and_then(from_str::<i64>).unwrap_or(10);
    if limit > 100 { return Err(human("cannot request more than 100 packages")) }
    let offset = (page - 1) * limit;
    let pattern = query.find_equiv(&"letter")
                       .map(|s| s.as_slice().char_at(0).to_lowercase())
                       .map(|s| format!("{}%", s))
                       .unwrap_or("%".to_string());

    // Collect all the packages
    let stmt = try!(conn.prepare("SELECT * FROM packages \
                                  WHERE name LIKE $3 \
                                  LIMIT $1 OFFSET $2"));
    let mut pkgs = Vec::new();
    for row in try!(stmt.query(&[&limit, &offset, &pattern])) {
        pkgs.push(Package::from_row(&row));
    }
    let pkgs = try!(Package::encode_many(conn, pkgs));

    // Query for the total count of packages
    let stmt = try!(conn.prepare("SELECT COUNT(*) FROM packages \
                                  WHERE name LIKE $1"));
    let row = try!(stmt.query(&[&pattern])).next().unwrap();
    let total = row.get(0u);

    #[deriving(Encodable)]
    struct R { packages: Vec<EncodablePackage>, meta: Meta }
    #[deriving(Encodable)]
    struct Meta { total: i64 }

    Ok(req.json(&R {
        packages: pkgs,
        meta: Meta { total: total },
    }))
}

pub fn summary(req: &mut Request) -> CargoResult<Response> {
    let tx = try!(req.tx());
    let num_packages = {
        let stmt = try!(tx.prepare("SELECT COUNT(*) FROM packages"));
        let mut rows = try!(stmt.query(&[]));
        rows.next().unwrap().get("count")
    };
    let num_downloads = {
        let stmt = try!(tx.prepare("SELECT total_downloads FROM metadata"));
        let mut rows = try!(stmt.query(&[]));
        rows.next().unwrap().get("total_downloads")
    };

    let to_pkgs = |stmt: PostgresStatement| {
        let rows = try!(stmt.query([]));
        Package::encode_many(tx, rows.map(|r| Package::from_row(&r)).collect())
    };
    let new_packages = try!(tx.prepare("SELECT * FROM packages \
                                        ORDER BY created_at DESC LIMIT 10"));
    let just_updated = try!(tx.prepare("SELECT * FROM packages \
                                        ORDER BY updated_at DESC LIMIT 10"));
    let most_downloaded = try!(tx.prepare("SELECT * FROM packages \
                                           ORDER BY downloads DESC LIMIT 10"));

    #[deriving(Encodable)]
    struct R {
        num_downloads: i64,
        num_packages: i64,
        new_packages: Vec<EncodablePackage>,
        most_downloaded: Vec<EncodablePackage>,
        just_updated: Vec<EncodablePackage>,
    }
    Ok(req.json(&R {
        num_downloads: num_downloads,
        num_packages: num_packages,
        new_packages: try!(to_pkgs(new_packages)),
        most_downloaded: try!(to_pkgs(most_downloaded)),
        just_updated: try!(to_pkgs(just_updated)),
    }))
}

pub fn show(req: &mut Request) -> CargoResult<Response> {
    let name = &req.params()["package_id"];
    let conn = try!(req.tx());
    let pkg = try!(Package::find_by_name(&*conn, name.as_slice()));
    let versions = try!(pkg.versions(&*conn));

    #[deriving(Encodable)]
    struct R { package: EncodablePackage, versions: Vec<EncodableVersion>, }
    Ok(req.json(&R {
        package: pkg.clone().encodable(versions.iter().map(|v| v.id).collect()),
        versions: versions.into_iter().map(|v| v.encodable(&pkg)).collect(),
    }))
}

#[deriving(Decodable)]
pub struct UpdateRequest { package: UpdatePackage }

#[deriving(Decodable)]
pub struct UpdatePackage {
    name: String,
}

pub fn update(req: &mut Request) -> CargoResult<Response> {
    try!(req.user());
    let conn = try!(req.tx());
    let name = &req.params()["package_id"];
    let pkg = try!(Package::find_by_name(&*conn, name.as_slice()));

    let update = conduit_json_parser::json_params::<UpdateRequest>(req).unwrap();
    // TODO: this should do something
    println!("new name: {}", update.package.name);

    #[deriving(Encodable)]
    struct R { package: EncodablePackage }
    Ok(req.json(&R { package: pkg.encodable(Vec::new()) }))
}

#[deriving(Encodable)]
pub struct NewPackage {
    pub name: String,
    pub vers: String,
    pub deps: Vec<Dependency>,
    pub cksum: String,
    pub features: HashMap<String, Vec<String>>,
}

pub fn new(req: &mut Request) -> CargoResult<Response> {
    let app = req.app().clone();

    let (mut new_pkg, user) = try!(parse_new_headers(req));

    // Persist the new package, if it doesn't already exist
    let pkg = try!(Package::find_or_insert(try!(req.tx()),
                                           new_pkg.name.as_slice(),
                                           user.id));
    if pkg.user_id != user.id {
        return Err(human("package name has already been claimed by another user"))
    }

    // Persist the new version of this package
    match try!(Version::find_by_num(try!(req.tx()), pkg.id,
                                    new_pkg.vers.as_slice())) {
        Some(..) => {
            return Err(human(format!("package version `{}` is already uploaded",
                                     new_pkg.vers)))
        }
        None => {}
    }
    let vers = try!(Version::insert(try!(req.tx()), pkg.id,
                                    new_pkg.vers.as_slice()));

    // Link this new version to all dependencies
    for dep in new_pkg.deps.iter() {
        let tx = try!(req.tx());
        let pkg = try!(Package::find_by_name(tx, dep.name.as_slice()).map_err(|_| {
            human(format!("no known package named `{}`", dep.name))
        }));
        try!(tx.execute("INSERT INTO version_dependencies \
                         (version_id, depends_on_id) VALUES ($1, $2)",
                        &[&vers.id, &pkg.id]));
    }

    // Upload the package to S3
    let handle = http::handle();
    let mut handle = match req.app().s3_proxy {
        Some(ref proxy) => handle.proxy(proxy.as_slice()),
        None => handle,
    };
    let path = pkg.s3_path(new_pkg.vers.as_slice());
    let (resp, cksum) = {
        let length = req.content_length().unwrap();
        let body = LimitErrorReader::new(req.body(), app.config.max_upload_size);
        let mut body = HashingReader::new(body);
        let resp = {
            let s3req = app.bucket.put(&mut handle, path.as_slice(), &mut body,
                                       "application/x-tar")
                                  .content_length(length)
                                  .header("Content-Encoding", "gzip");
            try!(s3req.exec().chain_error(|| {
                internal(format!("failed to upload to S3: `{}`", path))
            }))
        };
        (resp, body.final())
    };
    new_pkg.cksum = cksum.as_slice().to_hex();
    if resp.get_code() != 200 {
        return Err(internal(format!("failed to get a 200 response from S3: {}",
                                    resp)))
    }

    // If the git commands fail below, we shouldn't keep the package on the
    // server.
    struct Bomb { app: Arc<App>, path: Option<String>, handle: http::Handle }
    impl Drop for Bomb {
        fn drop(&mut self) {
            match self.path {
                Some(ref path) => {
                    let _ = self.app.bucket.delete(&mut self.handle,
                                                   path.as_slice())
                                .exec();
                }
                None => {}
            }
        }
    }
    let mut bomb = Bomb { app: app.clone(), path: Some(path), handle: handle };

    // Register this package in our local git repo.
    let pkg = try!(Package::find_by_name(try!(req.tx()),
                                         new_pkg.name.as_slice()));
    try!(git::add_package(&**req.app(), &new_pkg).chain_error(|| {
        internal(format!("could not add package `{}` to the git repo", pkg.name))
    }));

    // Now that we've come this far, we're committed!
    bomb.path = None;

    #[deriving(Encodable)]
    struct R { ok: bool, package: EncodablePackage }
    Ok(req.json(&R { ok: true, package: pkg.encodable(Vec::new()) }))
}

fn parse_new_headers(req: &mut Request) -> CargoResult<(NewPackage, User)> {
    // Peel out all input parameters
    fn header<'a>(req: &'a Request, hdr: &str) -> CargoResult<Vec<&'a str>> {
        req.headers().find(hdr).require(|| {
            human(format!("missing header: {}", hdr))
        })
    }
    let auth = try!(header(req, "X-Cargo-Auth"))[0].to_string();
    let name = try!(header(req, "X-Cargo-Pkg-Name"))[0].to_string();
    let vers = try!(header(req, "X-Cargo-Pkg-Version"))[0].to_string();
    let feat = try!(header(req, "X-Cargo-Pkg-Feature"))[0].to_string();
    let deps = try!(req.headers().find("X-Cargo-Pkg-Dep").unwrap_or(Vec::new())
                       .iter().flat_map(|s| s.as_slice().split(';'))
                       .map(Dependency::parse)
                       .collect::<CargoResult<Vec<_>>>());
    let feat = match json::decode(feat.as_slice()) {
        Ok(map) => map,
        Err(..) => return Err(human("malformed feature header")),
    };

    // Make sure the tarball being uploaded looks sane
    let length = try!(req.content_length().require(|| {
        human("missing header: Content-Length")
    }));
    let max = req.app().config.max_upload_size;
    if length > max { return Err(human(format!("max upload size is: {}", max))) }
    {
        let ty = try!(header(req, "Content-Type"))[0];
        if ty != "application/x-tar" {
            return Err(human(format!("expected `application/x-tar`, \
                                      found `{}`", ty)))
        }
        let enc = try!(header(req, "Content-Encoding"))[0];
        if enc != "gzip" && enc != "x-gzip" {
            return Err(human(format!("expected `gzip`, found `{}`", enc)))
        }
    }

    // Make sure the api token is a valid api token
    let user = try!(User::find_by_api_token(try!(req.tx()),
                                            auth.as_slice()).map_err(|_| {
        human("invalid or unknown auth token supplied")
    }));

    // Validate the name parameter and such
    let new_pkg = NewPackage {
        name: name.as_slice().chars().map(|c| c.to_lowercase()).collect(),
        vers: vers,
        deps: deps,
        features: feat,
        cksum: String::new(),
    };
    if !Package::valid_name(new_pkg.name.as_slice()) {
        return Err(human(format!("invalid package name: `{}`", new_pkg.name)))
    }
    if !Version::valid(new_pkg.vers.as_slice()) {
        return Err(human(format!("invalid package version: `{}`", new_pkg.vers)))
    }
    Ok((new_pkg, user))
}

pub fn download(req: &mut Request) -> CargoResult<Response> {
    let pkg_name = req.params()["package_id"].as_slice();
    let filename = req.params()["filename"].as_slice();
    if !filename.starts_with(pkg_name) || !filename.ends_with(".tar.gz") {
        return Err(human("download filename is not a tarball with the package \
                          name as a prefix"))
    }
    let version = filename.slice(pkg_name.len() + 1,
                                 filename.len() - ".tar.gz".len());
    let tx = try!(req.tx());
    let stmt = try!(tx.prepare("SELECT packages.id as package_id,
                                       versions.id as version_id
                                FROM packages
                                LEFT JOIN versions ON
                                    packages.id = versions.package_id
                                WHERE packages.name = $1
                                  AND versions.num = $2
                                LIMIT 1"));
    let mut rows = try!(stmt.query(&[&pkg_name as &ToSql, &version as &ToSql]));
    let row = try!(rows.next().require(|| human("package or version not found")));
    let package_id: i32 = row.get("package_id");
    let version_id: i32 = row.get("version_id");

    // Bump download counts.
    //
    // Note that this is *not* an atomic update, and that's somewhat
    // intentional. It doesn't appear that postgres supports an atomic update of
    // a counter, so we just do the hopefully "least racy" thing. This is
    // largely ok because these download counters are just that, counters. No
    // need to have super high-fidelity counter.
    try!(tx.execute("UPDATE packages SET downloads = downloads + 1
                     WHERE id = $1", &[&package_id]));
    try!(tx.execute("UPDATE versions SET downloads = downloads + 1
                     WHERE id = $1", &[&version_id]));
    try!(tx.execute("UPDATE metadata SET total_downloads = total_downloads + 1",
                    &[]));

    // Now that we've done our business, redirect to the actual data.
    let redirect_url = format!("https://{}/pkg/{}/{}-{}.tar.gz",
                               req.app().bucket.host(),
                               pkg_name, pkg_name, version);

    if req.wants_json() {
        #[deriving(Encodable)]
        struct R { ok: bool, url: String }
        Ok(req.json(&R{ ok: true, url: redirect_url }))
    } else {
        Ok(req.redirect(redirect_url))
    }
}
