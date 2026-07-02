use sql::token::{lex, Keyword, Token};

#[test]
fn lexes_keywords_idents_and_operators() {
    let toks = lex("SELECT id FROM t WHERE age >= 18").unwrap();
    assert_eq!(
        toks,
        vec![
            Token::Keyword(Keyword::Select),
            Token::Ident("id".into()),
            Token::Keyword(Keyword::From),
            Token::Ident("t".into()),
            Token::Keyword(Keyword::Where),
            Token::Ident("age".into()),
            Token::Ge,
            Token::Int(18),
            Token::Eof,
        ]
    );
}

#[test]
fn lexes_string_with_escaped_quote() {
    let toks = lex("'it''s'").unwrap();
    assert_eq!(toks, vec![Token::Str("it's".into()), Token::Eof]);
}

#[test]
fn lexes_float_and_ne_variants() {
    assert_eq!(lex("3.5").unwrap()[0], Token::Float(3.5));
    assert_eq!(lex("a <> b").unwrap()[1], Token::Ne);
    assert_eq!(lex("a != b").unwrap()[1], Token::Ne);
}

#[test]
fn keywords_are_case_insensitive() {
    assert_eq!(lex("select").unwrap()[0], Token::Keyword(Keyword::Select));
    assert_eq!(lex("SeLeCt").unwrap()[0], Token::Keyword(Keyword::Select));
}

#[test]
fn unterminated_string_errors() {
    assert!(lex("'oops").is_err());
}
