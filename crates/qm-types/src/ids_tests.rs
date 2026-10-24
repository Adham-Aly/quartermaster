use super::{CorrelationId, CrewmateId, SecondmateId, TaskLabelId};

#[test]
fn ids_show_their_current_prefix_and_parse_their_older_one() {
    let sm: SecondmateId = "S-2".parse().unwrap();
    assert_eq!(sm.to_string(), "S-02");
    assert_eq!("SM-02".parse::<SecondmateId>().unwrap(), sm);
    let corr: CorrelationId = "R-193".parse().unwrap();
    assert_eq!(corr.to_string(), "R-0193");
    assert_eq!("C-0193".parse::<CorrelationId>().unwrap(), corr);
    let crewmate: CrewmateId = "C-42".parse().unwrap();
    assert_eq!(crewmate.to_string(), "C-0042");
    assert_eq!("T-0042".parse::<CrewmateId>().unwrap(), crewmate);
    assert_eq!("T-17".parse::<TaskLabelId>().unwrap().to_string(), "T-0017");
    assert!(
        "C-0017".parse::<TaskLabelId>().is_err(),
        "a crewmate isn't a task"
    );
    for wrong in ["X-02", "S02", "S-", "S-0", "SMM-2", "T-4a"] {
        assert!(wrong.parse::<SecondmateId>().is_err(), "{wrong} parsed");
    }
    assert!("T-0042".parse::<CorrelationId>().is_err());
    assert!("R-0042".parse::<CrewmateId>().is_err());
}
