//! Embedded ISO 639-2 language table. Media containers tag tracks with
//! ISO 639-2 codes, almost always the /B (bibliographic) form, so `code`
//! holds /B and is the canonical form persisted in config; `code_t` holds
//! the /T (terminological) form for the ~20 languages where it differs, and
//! `code_1` the two-letter ISO 639-1 code where one exists. Keeping every
//! form lets mpv's alang/slang matching work regardless of how a given file
//! was tagged.

pub struct Language {
    /// ISO 639-2/B code — the canonical form stored in config.
    pub code: &'static str,
    /// ISO 639-2/T code, only where it differs from /B.
    pub code_t: Option<&'static str>,
    /// ISO 639-1 two-letter code, where one exists.
    pub code_1: Option<&'static str>,
    pub name: &'static str,
}

const fn lang(
    code: &'static str,
    code_t: Option<&'static str>,
    code_1: Option<&'static str>,
    name: &'static str,
) -> Language {
    Language {
        code,
        code_t,
        code_1,
        name,
    }
}

/// Every ISO 639-2 language with an ISO 639-1 code, plus Filipino, sorted by
/// English name. This covers what real-world media is tagged with without
/// dragging in the several hundred 639-2 collective/family codes.
pub const LANGUAGES: &[Language] = &[
    lang("abk", None, Some("ab"), "Abkhazian"),
    lang("aar", None, Some("aa"), "Afar"),
    lang("afr", None, Some("af"), "Afrikaans"),
    lang("aka", None, Some("ak"), "Akan"),
    lang("alb", Some("sqi"), Some("sq"), "Albanian"),
    lang("amh", None, Some("am"), "Amharic"),
    lang("ara", None, Some("ar"), "Arabic"),
    lang("arg", None, Some("an"), "Aragonese"),
    lang("arm", Some("hye"), Some("hy"), "Armenian"),
    lang("asm", None, Some("as"), "Assamese"),
    lang("ava", None, Some("av"), "Avaric"),
    lang("ave", None, Some("ae"), "Avestan"),
    lang("aym", None, Some("ay"), "Aymara"),
    lang("aze", None, Some("az"), "Azerbaijani"),
    lang("bam", None, Some("bm"), "Bambara"),
    lang("bak", None, Some("ba"), "Bashkir"),
    lang("baq", Some("eus"), Some("eu"), "Basque"),
    lang("bel", None, Some("be"), "Belarusian"),
    lang("ben", None, Some("bn"), "Bengali"),
    lang("bis", None, Some("bi"), "Bislama"),
    lang("bos", None, Some("bs"), "Bosnian"),
    lang("bre", None, Some("br"), "Breton"),
    lang("bul", None, Some("bg"), "Bulgarian"),
    lang("bur", Some("mya"), Some("my"), "Burmese"),
    lang("cat", None, Some("ca"), "Catalan"),
    lang("cha", None, Some("ch"), "Chamorro"),
    lang("che", None, Some("ce"), "Chechen"),
    lang("nya", None, Some("ny"), "Chichewa"),
    lang("chi", Some("zho"), Some("zh"), "Chinese"),
    lang("chu", None, Some("cu"), "Church Slavonic"),
    lang("chv", None, Some("cv"), "Chuvash"),
    lang("cor", None, Some("kw"), "Cornish"),
    lang("cos", None, Some("co"), "Corsican"),
    lang("cre", None, Some("cr"), "Cree"),
    lang("hrv", None, Some("hr"), "Croatian"),
    lang("cze", Some("ces"), Some("cs"), "Czech"),
    lang("dan", None, Some("da"), "Danish"),
    lang("div", None, Some("dv"), "Divehi"),
    lang("dut", Some("nld"), Some("nl"), "Dutch"),
    lang("dzo", None, Some("dz"), "Dzongkha"),
    lang("eng", None, Some("en"), "English"),
    lang("epo", None, Some("eo"), "Esperanto"),
    lang("est", None, Some("et"), "Estonian"),
    lang("ewe", None, Some("ee"), "Ewe"),
    lang("fao", None, Some("fo"), "Faroese"),
    lang("fij", None, Some("fj"), "Fijian"),
    lang("fil", None, None, "Filipino"),
    lang("fin", None, Some("fi"), "Finnish"),
    lang("fre", Some("fra"), Some("fr"), "French"),
    lang("ful", None, Some("ff"), "Fulah"),
    lang("glg", None, Some("gl"), "Galician"),
    lang("lug", None, Some("lg"), "Ganda"),
    lang("geo", Some("kat"), Some("ka"), "Georgian"),
    lang("ger", Some("deu"), Some("de"), "German"),
    lang("gre", Some("ell"), Some("el"), "Greek"),
    lang("grn", None, Some("gn"), "Guarani"),
    lang("guj", None, Some("gu"), "Gujarati"),
    lang("hat", None, Some("ht"), "Haitian Creole"),
    lang("hau", None, Some("ha"), "Hausa"),
    lang("heb", None, Some("he"), "Hebrew"),
    lang("her", None, Some("hz"), "Herero"),
    lang("hin", None, Some("hi"), "Hindi"),
    lang("hmo", None, Some("ho"), "Hiri Motu"),
    lang("hun", None, Some("hu"), "Hungarian"),
    lang("ice", Some("isl"), Some("is"), "Icelandic"),
    lang("ido", None, Some("io"), "Ido"),
    lang("ibo", None, Some("ig"), "Igbo"),
    lang("ind", None, Some("id"), "Indonesian"),
    lang("ina", None, Some("ia"), "Interlingua"),
    lang("ile", None, Some("ie"), "Interlingue"),
    lang("iku", None, Some("iu"), "Inuktitut"),
    lang("ipk", None, Some("ik"), "Inupiaq"),
    lang("gle", None, Some("ga"), "Irish"),
    lang("ita", None, Some("it"), "Italian"),
    lang("jpn", None, Some("ja"), "Japanese"),
    lang("jav", None, Some("jv"), "Javanese"),
    lang("kal", None, Some("kl"), "Kalaallisut"),
    lang("kan", None, Some("kn"), "Kannada"),
    lang("kau", None, Some("kr"), "Kanuri"),
    lang("kas", None, Some("ks"), "Kashmiri"),
    lang("kaz", None, Some("kk"), "Kazakh"),
    lang("khm", None, Some("km"), "Khmer"),
    lang("kik", None, Some("ki"), "Kikuyu"),
    lang("kin", None, Some("rw"), "Kinyarwanda"),
    lang("kom", None, Some("kv"), "Komi"),
    lang("kon", None, Some("kg"), "Kongo"),
    lang("kor", None, Some("ko"), "Korean"),
    lang("kua", None, Some("kj"), "Kuanyama"),
    lang("kur", None, Some("ku"), "Kurdish"),
    lang("kir", None, Some("ky"), "Kyrgyz"),
    lang("lao", None, Some("lo"), "Lao"),
    lang("lat", None, Some("la"), "Latin"),
    lang("lav", None, Some("lv"), "Latvian"),
    lang("lim", None, Some("li"), "Limburgish"),
    lang("lin", None, Some("ln"), "Lingala"),
    lang("lit", None, Some("lt"), "Lithuanian"),
    lang("lub", None, Some("lu"), "Luba-Katanga"),
    lang("ltz", None, Some("lb"), "Luxembourgish"),
    lang("mac", Some("mkd"), Some("mk"), "Macedonian"),
    lang("mlg", None, Some("mg"), "Malagasy"),
    lang("may", Some("msa"), Some("ms"), "Malay"),
    lang("mal", None, Some("ml"), "Malayalam"),
    lang("mlt", None, Some("mt"), "Maltese"),
    lang("glv", None, Some("gv"), "Manx"),
    lang("mao", Some("mri"), Some("mi"), "Maori"),
    lang("mar", None, Some("mr"), "Marathi"),
    lang("mah", None, Some("mh"), "Marshallese"),
    lang("mon", None, Some("mn"), "Mongolian"),
    lang("nau", None, Some("na"), "Nauru"),
    lang("nav", None, Some("nv"), "Navajo"),
    lang("ndo", None, Some("ng"), "Ndonga"),
    lang("nep", None, Some("ne"), "Nepali"),
    lang("nde", None, Some("nd"), "North Ndebele"),
    lang("sme", None, Some("se"), "Northern Sami"),
    lang("nor", None, Some("no"), "Norwegian"),
    lang("nob", None, Some("nb"), "Norwegian Bokmal"),
    lang("nno", None, Some("nn"), "Norwegian Nynorsk"),
    lang("oci", None, Some("oc"), "Occitan"),
    lang("ori", None, Some("or"), "Odia"),
    lang("oji", None, Some("oj"), "Ojibwa"),
    lang("orm", None, Some("om"), "Oromo"),
    lang("oss", None, Some("os"), "Ossetian"),
    lang("pli", None, Some("pi"), "Pali"),
    lang("pus", None, Some("ps"), "Pashto"),
    lang("per", Some("fas"), Some("fa"), "Persian"),
    lang("pol", None, Some("pl"), "Polish"),
    lang("por", None, Some("pt"), "Portuguese"),
    lang("pan", None, Some("pa"), "Punjabi"),
    lang("que", None, Some("qu"), "Quechua"),
    lang("rum", Some("ron"), Some("ro"), "Romanian"),
    lang("roh", None, Some("rm"), "Romansh"),
    lang("run", None, Some("rn"), "Rundi"),
    lang("rus", None, Some("ru"), "Russian"),
    lang("smo", None, Some("sm"), "Samoan"),
    lang("sag", None, Some("sg"), "Sango"),
    lang("san", None, Some("sa"), "Sanskrit"),
    lang("srd", None, Some("sc"), "Sardinian"),
    lang("srp", None, Some("sr"), "Serbian"),
    lang("sna", None, Some("sn"), "Shona"),
    lang("iii", None, Some("ii"), "Sichuan Yi"),
    lang("snd", None, Some("sd"), "Sindhi"),
    lang("sin", None, Some("si"), "Sinhala"),
    lang("slo", Some("slk"), Some("sk"), "Slovak"),
    lang("slv", None, Some("sl"), "Slovenian"),
    lang("som", None, Some("so"), "Somali"),
    lang("nbl", None, Some("nr"), "South Ndebele"),
    lang("sot", None, Some("st"), "Southern Sotho"),
    lang("spa", None, Some("es"), "Spanish"),
    lang("sun", None, Some("su"), "Sundanese"),
    lang("swa", None, Some("sw"), "Swahili"),
    lang("ssw", None, Some("ss"), "Swati"),
    lang("swe", None, Some("sv"), "Swedish"),
    lang("tgl", None, Some("tl"), "Tagalog"),
    lang("tah", None, Some("ty"), "Tahitian"),
    lang("tgk", None, Some("tg"), "Tajik"),
    lang("tam", None, Some("ta"), "Tamil"),
    lang("tat", None, Some("tt"), "Tatar"),
    lang("tel", None, Some("te"), "Telugu"),
    lang("tha", None, Some("th"), "Thai"),
    lang("tib", Some("bod"), Some("bo"), "Tibetan"),
    lang("tir", None, Some("ti"), "Tigrinya"),
    lang("ton", None, Some("to"), "Tonga"),
    lang("tso", None, Some("ts"), "Tsonga"),
    lang("tsn", None, Some("tn"), "Tswana"),
    lang("tur", None, Some("tr"), "Turkish"),
    lang("tuk", None, Some("tk"), "Turkmen"),
    lang("twi", None, Some("tw"), "Twi"),
    lang("ukr", None, Some("uk"), "Ukrainian"),
    lang("urd", None, Some("ur"), "Urdu"),
    lang("uig", None, Some("ug"), "Uyghur"),
    lang("uzb", None, Some("uz"), "Uzbek"),
    lang("ven", None, Some("ve"), "Venda"),
    lang("vie", None, Some("vi"), "Vietnamese"),
    lang("vol", None, Some("vo"), "Volapuk"),
    lang("wln", None, Some("wa"), "Walloon"),
    lang("wel", Some("cym"), Some("cy"), "Welsh"),
    lang("fry", None, Some("fy"), "Western Frisian"),
    lang("wol", None, Some("wo"), "Wolof"),
    lang("xho", None, Some("xh"), "Xhosa"),
    lang("yid", None, Some("yi"), "Yiddish"),
    lang("yor", None, Some("yo"), "Yoruba"),
    lang("zha", None, Some("za"), "Zhuang"),
    lang("zul", None, Some("zu"), "Zulu"),
];

/// Look a language up by any of its code forms, case-insensitively, so a
/// container tagged "deu" or "de" still resolves to the same entry as "ger".
pub fn find(code: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|lang| {
        lang.code.eq_ignore_ascii_case(code)
            || lang.code_t.is_some_and(|t| t.eq_ignore_ascii_case(code))
            || lang.code_1.is_some_and(|c| c.eq_ignore_ascii_case(code))
    })
}

/// The /B code for any recognised form. Unrecognised tags pass through
/// lowercased so an oddly tagged track still round-trips into alang/slang
/// matching instead of being dropped.
pub fn canonical(code: &str) -> String {
    match find(code) {
        Some(lang) => lang.code.to_string(),
        None => code.to_ascii_lowercase(),
    }
}

/// English name for a code, for row summaries like "Italian (ita)".
pub fn name(code: &str) -> Option<&'static str> {
    find(code).map(|lang| lang.name)
}

/// The value handed to mpv's alang/slang: every known form of the code, /B
/// first (e.g. "ger,deu,de"), so track matching works no matter which form
/// the container used. Unrecognised codes pass through.
pub fn mpv_lang_list(code: &str) -> String {
    let Some(lang) = find(code) else {
        return code.to_ascii_lowercase();
    };
    let mut list = String::from(lang.code);
    for form in [lang.code_t, lang.code_1].into_iter().flatten() {
        list.push(',');
        list.push_str(form);
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_complete_and_sorted() {
        assert!(LANGUAGES.len() >= 180);
        for pair in LANGUAGES.windows(2) {
            assert!(
                pair[0].name < pair[1].name,
                "{} should sort before {}",
                pair[0].name,
                pair[1].name
            );
        }
    }

    #[test]
    fn no_duplicate_codes() {
        let mut seen = std::collections::HashSet::new();
        for lang in LANGUAGES {
            for code in [Some(lang.code), lang.code_t, lang.code_1]
                .into_iter()
                .flatten()
            {
                assert!(seen.insert(code), "duplicate code {code}");
            }
        }
    }

    #[test]
    fn find_matches_every_form() {
        for code in ["ger", "deu", "de", "GER", "De"] {
            assert_eq!(find(code).map(|l| l.name), Some("German"), "{code}");
        }
        assert!(find("zz").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn canonical_prefers_b_form() {
        assert_eq!(canonical("deu"), "ger");
        assert_eq!(canonical("IT"), "ita");
        assert_eq!(canonical("x-unknown"), "x-unknown");
    }

    #[test]
    fn mpv_lang_list_emits_all_forms() {
        assert_eq!(mpv_lang_list("fre"), "fre,fra,fr");
        assert_eq!(mpv_lang_list("ita"), "ita,it");
        assert_eq!(mpv_lang_list("fil"), "fil");
        assert_eq!(mpv_lang_list("weird"), "weird");
    }
}
