//! Tests against the real engine. They download ~19 MB on first run.
//! cargo test --release -- --ignored

use whistle::{Error, Options, Whistle, MAX_SAMPLES};

fn model() -> Whistle {
    Whistle::load_default().expect("load whistle")
}

#[test]
#[ignore = "needs the engine and weights"]
fn silence_is_empty() {
    let t = model()
        .transcribe(&vec![0.0; 16_000 * 3], &Options::default())
        .unwrap();
    assert!(t.is_empty(), "{t:?}");
    assert_eq!(t.language, None);
}

#[test]
#[ignore = "needs the engine and weights"]
fn empty_input_is_ok() {
    assert!(model()
        .transcribe(&[], &Options::default())
        .unwrap()
        .is_empty());
}

#[test]
#[ignore = "needs the engine and weights"]
fn rejects_over_30s() {
    let err = model()
        .transcribe(&vec![0.0; MAX_SAMPLES + 1], &Options::default())
        .unwrap_err();
    assert!(matches!(err, Error::TooLong(_)));
}

#[test]
#[ignore = "needs the engine and weights"]
fn long_silence_in_windows() {
    let t = model()
        .transcribe_long(&vec![0.0; MAX_SAMPLES * 2 + 100], &Options::default())
        .unwrap();
    assert!(t.is_empty());
}

#[test]
#[ignore = "needs the engine and weights"]
fn embedding_rows() {
    let w = model();
    let dim = w.embed_dim().unwrap();
    assert!(dim > 0);
    let e = w.embed(&vec![0.0; 16_000]).unwrap();
    assert_eq!(e.len() % dim, 0, "len {} dim {dim}", e.len());
}

#[test]
#[ignore = "needs the engine and weights"]
fn same_weights_reuse_handle() {
    let a = model();
    let b = Whistle::load_default().unwrap();
    assert_eq!(a.weights(), b.weights());
}
