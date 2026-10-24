fn main() {
    let _ = gw.call_json("alpha.one", &[], None, None, secs(30));
    // NO_ROUTE: alpha.old is a legacy route kept for documentation only
    let _ = gw.call_json("alpha.old", &[], None, None, secs(30));
}
