use super::*;

fn config(text: &str) -> gix::config::File {
    gix::config::File::try_from(text).unwrap()
}

#[test]
fn user_email_is_read_from_the_config_and_trimmed() {
    assert_eq!(
        user_email(&config(
            "[user]\n\tname = A\n\temail = \"me@example.test\"\n"
        )),
        "me@example.test"
    );
}

#[test]
fn missing_user_email_is_empty() {
    assert_eq!(user_email(&config("[user]\n\tname = A\n")), "");
    assert_eq!(user_email(&config("")), "");
}

#[test]
fn later_config_wins_like_git_config() {
    let text = "[user]\n\temail = first@example.test\n[user]\n\temail = second@example.test\n";
    assert_eq!(user_email(&config(text)), "second@example.test");
}
