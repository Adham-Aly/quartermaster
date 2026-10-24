use super::{CorrelationId, SecondmateId, TaskId};

#[test]
fn ids_show_their_current_prefix_and_parse_their_older_one() {
    let sm: SecondmateId = "S-2".parse().unwrap();
    assert_eq!(sm.to_string(), "S-02");
    assert_eq!("SM-02".parse::<SecondmateId>().unwrap(), sm);
    let corr: CorrelationId = "R-193".parse().unwrap();
    assert_eq!(corr.to_string(), "R-0193");
    assert_eq!("C-0193".parse::<CorrelationId>().unwrap(), corr);
    assert_eq!("T-42".parse::<TaskId>().unwrap().to_string(), "T-0042");
    for wrong in ["X-02", "S02", "S-", "S-0", "SMM-2", "T-4a"] {
        assert!(wrong.parse::<SecondmateId>().is_err(), "{wrong} parsed");
    }
    assert!("T-0042".parse::<CorrelationId>().is_err());
}
