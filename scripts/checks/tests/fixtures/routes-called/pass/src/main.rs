fn main() {
    let _ = gw.call_json("alpha.one", &[], None, None, secs(30));
    let _ = gw.call_json(
        "beta.two",
        &[], None, None, secs(30));
}
