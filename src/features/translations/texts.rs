//! System texts: the words DumbMail itself puts in front of people (the unsubscribe and preference
//! pages, the double opt-in email and page, the footer lines added to email), in English, Spanish,
//! French, German, Italian, Portuguese and Dutch, and as each account rewords them.
//!
//! A page is shown in the person's language: their profile's, else the browser's
//! (`Accept-Language`), else the account's default; only languages the account sends in are used.
//! An account that never set up languages and never reworded a text gets exactly the English it
//! always did. Each account's settings are cached for [`CACHE_SECS`] seconds per server.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::locale;

pub const CACHE_SECS: u64 = 60;
/// The longest reworded text.
pub const MAX_TEXT: usize = 500;

/// Every system text: (key, what it is, English).
pub const KEYS: &[(&str, &str, &str)] = &[
    ("unsub.title", "Unsubscribe page: title and heading", "Unsubscribe"),
    ("unsub.email_question", "Unsubscribe page: question (email)", "Stop receiving marketing emails from {name}?"),
    ("unsub.sms_question", "Unsubscribe page: question (texts)", "Stop receiving text messages from {name}?"),
    ("unsub.button", "Unsubscribe page: button", "Unsubscribe"),
    ("unsub.done_page", "After unsubscribing: page title", "Unsubscribed"),
    ("unsub.done_title", "After unsubscribing: heading", "You're unsubscribed"),
    ("unsub.done_body", "After unsubscribing: message", "You won't get marketing messages from {name} anymore."),
    ("prefs.title", "Preference page: title and heading", "Email preferences"),
    ("prefs.intro", "Preference page: first line", "Choose what you'd like to get from {name}."),
    ("prefs.save", "Preference page: save button", "Save preferences"),
    ("prefs.all", "Preference page: unsubscribe-from-all button", "Unsubscribe from all emails"),
    ("prefs.saved", "Preference page: saved notice", "Saved. Your preferences are updated."),
    ("prefs.test", "Preference page: notice for a test email", "This was a test email, so nothing changed."),
    ("prefs.error", "Preference page: error notice", "Something went wrong. Please try again."),
    ("confirm.title", "Double opt-in: page title, heading and email heading", "Confirm your subscription"),
    ("confirm.question", "Double opt-in page: question", "Start getting emails from {name}?"),
    ("confirm.button", "Double opt-in: button (page and email)", "Yes, subscribe me"),
    ("confirm.done_title", "Double opt-in page: confirmed heading", "You're subscribed"),
    ("confirm.done_body", "Double opt-in page: confirmed message", "Thanks! You'll start getting emails from {name}."),
    ("confirm.code", "Double opt-in page: before a coupon code", "Your code:"),
    ("confirm.expired_title", "Double opt-in page: expired link heading", "This link has expired"),
    ("confirm.expired_body", "Double opt-in page: expired link message", "Sign up again and we'll send you a new one."),
    ("confirm.email_subject", "Double opt-in email: subject", "Confirm your subscription to {name}"),
    ("confirm.email_line", "Double opt-in email: line above the button", "Tap the button to start getting emails from {name}."),
    ("confirm.email_text_line", "Double opt-in email: plain-text line before the link", "Open this link to start getting emails from {name}"),
    ("confirm.email_ignore", "Double opt-in email: last line", "If you didn't sign up, ignore this email and you won't hear from us. The link works for {days} days."),
    ("footer.reason", "Email footer: why they get it", "You're receiving this because you subscribed."),
    ("footer.unsubscribe", "Email footer: unsubscribe link", "Unsubscribe"),
];

/// Languages with shipped translations of every system text.
pub const SHIPPED: [&str; 7] = ["en", "es", "fr", "de", "it", "pt", "nl"];

fn shipped(lang: &str) -> Option<&'static [&'static str; 28]> {
    Some(match lang {
        "es" => &ES,
        "fr" => &FR,
        "de" => &DE,
        "it" => &IT,
        "pt" => &PT,
        "nl" => &NL,
        _ => return None,
    })
}

// In the order of KEYS.
const ES: [&str; 28] = [
    "Cancelar suscripción",
    "¿Dejar de recibir correos de marketing de {name}?",
    "¿Dejar de recibir mensajes de texto de {name}?",
    "Cancelar suscripción",
    "Suscripción cancelada",
    "Has cancelado tu suscripción",
    "Ya no recibirás mensajes de marketing de {name}.",
    "Preferencias de correo",
    "Elige qué quieres recibir de {name}.",
    "Guardar preferencias",
    "Cancelar la suscripción a todos los correos",
    "Guardado. Tus preferencias se han actualizado.",
    "Este era un correo de prueba, así que no ha cambiado nada.",
    "Algo salió mal. Inténtalo de nuevo.",
    "Confirma tu suscripción",
    "¿Quieres empezar a recibir correos de {name}?",
    "Sí, suscribirme",
    "Te has suscrito",
    "¡Gracias! Empezarás a recibir correos de {name}.",
    "Tu código:",
    "Este enlace ha caducado",
    "Vuelve a suscribirte y te enviaremos uno nuevo.",
    "Confirma tu suscripción a {name}",
    "Toca el botón para empezar a recibir correos de {name}.",
    "Abre este enlace para empezar a recibir correos de {name}",
    "Si no te has suscrito, ignora este correo y no volverás a saber de nosotros. El enlace funciona durante {days} días.",
    "Recibes este correo porque te suscribiste.",
    "Cancelar suscripción",
];

const FR: [&str; 28] = [
    "Se désabonner",
    "Ne plus recevoir les e-mails marketing de {name} ?",
    "Ne plus recevoir les SMS de {name} ?",
    "Se désabonner",
    "Désabonnement confirmé",
    "Vous êtes désabonné(e)",
    "Vous ne recevrez plus de messages marketing de {name}.",
    "Préférences e-mail",
    "Choisissez ce que vous souhaitez recevoir de {name}.",
    "Enregistrer mes préférences",
    "Me désabonner de tous les e-mails",
    "C'est enregistré. Vos préférences sont à jour.",
    "Il s'agissait d'un e-mail de test, rien n'a donc été modifié.",
    "Une erreur s'est produite. Veuillez réessayer.",
    "Confirmez votre inscription",
    "Commencer à recevoir les e-mails de {name} ?",
    "Oui, je m'inscris",
    "Vous êtes inscrit(e)",
    "Merci ! Vous allez commencer à recevoir les e-mails de {name}.",
    "Votre code :",
    "Ce lien a expiré",
    "Inscrivez-vous à nouveau et nous vous en enverrons un nouveau.",
    "Confirmez votre inscription à {name}",
    "Appuyez sur le bouton pour commencer à recevoir les e-mails de {name}.",
    "Ouvrez ce lien pour commencer à recevoir les e-mails de {name}",
    "Si vous ne vous êtes pas inscrit(e), ignorez cet e-mail : vous n'aurez plus de nouvelles de nous. Le lien est valable {days} jours.",
    "Vous recevez cet e-mail parce que vous vous êtes inscrit(e).",
    "Se désabonner",
];

const DE: [&str; 28] = [
    "Abmelden",
    "Keine Marketing-E-Mails mehr von {name} erhalten?",
    "Keine SMS mehr von {name} erhalten?",
    "Abmelden",
    "Abgemeldet",
    "Sie sind abgemeldet",
    "Sie erhalten keine Marketing-Nachrichten mehr von {name}.",
    "E-Mail-Einstellungen",
    "Wählen Sie, was Sie von {name} erhalten möchten.",
    "Einstellungen speichern",
    "Von allen E-Mails abmelden",
    "Gespeichert. Ihre Einstellungen sind aktualisiert.",
    "Dies war eine Test-E-Mail, daher wurde nichts geändert.",
    "Etwas ist schiefgelaufen. Bitte versuchen Sie es erneut.",
    "Bestätigen Sie Ihr Abonnement",
    "Ab jetzt E-Mails von {name} erhalten?",
    "Ja, ich möchte mich anmelden",
    "Sie sind angemeldet",
    "Danke! Sie erhalten ab jetzt E-Mails von {name}.",
    "Ihr Code:",
    "Dieser Link ist abgelaufen",
    "Melden Sie sich erneut an, dann senden wir Ihnen einen neuen.",
    "Bestätigen Sie Ihr Abonnement bei {name}",
    "Tippen Sie auf die Schaltfläche, um ab jetzt E-Mails von {name} zu erhalten.",
    "Öffnen Sie diesen Link, um ab jetzt E-Mails von {name} zu erhalten",
    "Wenn Sie sich nicht angemeldet haben, ignorieren Sie diese E-Mail. Sie hören dann nichts mehr von uns. Der Link ist {days} Tage gültig.",
    "Sie erhalten diese E-Mail, weil Sie sich angemeldet haben.",
    "Abmelden",
];

const IT: [&str; 28] = [
    "Annulla l'iscrizione",
    "Non vuoi più ricevere email di marketing da {name}?",
    "Non vuoi più ricevere SMS da {name}?",
    "Annulla l'iscrizione",
    "Iscrizione annullata",
    "La tua iscrizione è stata annullata",
    "Non riceverai più messaggi di marketing da {name}.",
    "Preferenze email",
    "Scegli cosa vuoi ricevere da {name}.",
    "Salva le preferenze",
    "Annulla l'iscrizione a tutte le email",
    "Salvato. Le tue preferenze sono state aggiornate.",
    "Questa era un'email di prova, quindi non è cambiato nulla.",
    "Qualcosa è andato storto. Riprova.",
    "Conferma la tua iscrizione",
    "Vuoi iniziare a ricevere email da {name}?",
    "Sì, iscrivimi",
    "Iscrizione confermata",
    "Grazie! Inizierai a ricevere email da {name}.",
    "Il tuo codice:",
    "Questo link è scaduto",
    "Iscriviti di nuovo e te ne invieremo uno nuovo.",
    "Conferma la tua iscrizione a {name}",
    "Tocca il pulsante per iniziare a ricevere email da {name}.",
    "Apri questo link per iniziare a ricevere email da {name}",
    "Se non ti sei iscritto, ignora questa email e non riceverai altro da noi. Il link è valido per {days} giorni.",
    "Ricevi questa email perché ti sei iscritto.",
    "Annulla l'iscrizione",
];

const PT: [&str; 28] = [
    "Cancelar inscrição",
    "Deixar de receber e-mails de marketing de {name}?",
    "Deixar de receber mensagens de texto de {name}?",
    "Cancelar inscrição",
    "Inscrição cancelada",
    "Sua inscrição foi cancelada",
    "Você não vai mais receber mensagens de marketing de {name}.",
    "Preferências de e-mail",
    "Escolha o que você quer receber de {name}.",
    "Salvar preferências",
    "Cancelar a inscrição em todos os e-mails",
    "Salvo. Suas preferências foram atualizadas.",
    "Este era um e-mail de teste, então nada mudou.",
    "Algo deu errado. Tente novamente.",
    "Confirme sua inscrição",
    "Começar a receber e-mails de {name}?",
    "Sim, quero me inscrever",
    "Inscrição confirmada",
    "Obrigado! Você vai começar a receber e-mails de {name}.",
    "Seu código:",
    "Este link expirou",
    "Inscreva-se de novo e enviaremos um novo link.",
    "Confirme sua inscrição em {name}",
    "Toque no botão para começar a receber e-mails de {name}.",
    "Abra este link para começar a receber e-mails de {name}",
    "Se você não se inscreveu, ignore este e-mail e não entraremos mais em contato. O link funciona por {days} dias.",
    "Você está recebendo este e-mail porque se inscreveu.",
    "Cancelar inscrição",
];

const NL: [&str; 28] = [
    "Afmelden",
    "Geen marketingmails meer ontvangen van {name}?",
    "Geen sms-berichten meer ontvangen van {name}?",
    "Afmelden",
    "Afgemeld",
    "Je bent afgemeld",
    "Je ontvangt geen marketingberichten meer van {name}.",
    "E-mailvoorkeuren",
    "Kies wat je van {name} wilt ontvangen.",
    "Voorkeuren opslaan",
    "Afmelden voor alle e-mails",
    "Opgeslagen. Je voorkeuren zijn bijgewerkt.",
    "Dit was een testmail, dus er is niets veranderd.",
    "Er ging iets mis. Probeer het opnieuw.",
    "Bevestig je inschrijving",
    "Wil je e-mails van {name} ontvangen?",
    "Ja, schrijf me in",
    "Je bent ingeschreven",
    "Bedankt! Je ontvangt vanaf nu e-mails van {name}.",
    "Je code:",
    "Deze link is verlopen",
    "Schrijf je opnieuw in, dan sturen we je een nieuwe.",
    "Bevestig je inschrijving bij {name}",
    "Tik op de knop om e-mails van {name} te gaan ontvangen.",
    "Open deze link om e-mails van {name} te gaan ontvangen",
    "Heb je je niet ingeschreven? Negeer dan deze e-mail en je hoort niets meer van ons. De link werkt {days} dagen.",
    "Je ontvangt deze e-mail omdat je je hebt ingeschreven.",
    "Afmelden",
];

fn key_index(key: &str) -> Option<usize> {
    KEYS.iter().position(|(k, _, _)| *k == key)
}

/// The shipped text for `key` in `lang` (its language part), if there is one.
pub fn shipped_text(lang: &str, key: &str) -> Option<&'static str> {
    let i = key_index(key)?;
    match locale::primary(lang) {
        "en" => Some(KEYS[i].2),
        p => shipped(p).map(|t| t[i]),
    }
}

/// The placeholders a text uses (`{name}`, `{days}`), sorted.
pub fn placeholders(text: &str) -> Vec<&str> {
    let mut v: Vec<&str> = ["{name}", "{days}"].into_iter().filter(|p| text.contains(p)).collect();
    v.sort();
    v
}

/// An account's language settings and reworded texts.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub default_language: String,
    pub languages: Vec<String>,
    /// (language, key) → text.
    pub texts: HashMap<(String, String), String>,
}

impl Settings {
    /// The default and the other languages.
    pub fn all(&self) -> Vec<String> {
        let mut v = vec![self.default_language.clone()];
        v.extend(self.languages.iter().cloned());
        v
    }
}

static CACHE: Lazy<Mutex<HashMap<Uuid, (Instant, Settings)>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Forget an account's cached settings (after a change on this server).
pub fn forget(account_id: Uuid) {
    if let Ok(mut c) = CACHE.lock() {
        c.remove(&account_id);
    }
}

static REFRESHING: Lazy<Mutex<std::collections::HashSet<Uuid>>> = Lazy::new(|| Mutex::new(std::collections::HashSet::new()));

/// Read an account's settings (two statements) on `conn`.
pub async fn load(conn: &mut sqlx::PgConnection, account_id: Uuid) -> sqlx::Result<Settings> {
    let row: Option<(String, Vec<String>)> = sqlx::query_as("SELECT default_language, languages FROM account_languages WHERE account_id = $1").bind(account_id).fetch_optional(&mut *conn).await?;
    let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT language, key, text FROM system_texts WHERE account_id = $1 LIMIT 2000").bind(account_id).fetch_all(&mut *conn).await?;
    let (default_language, languages) = row.unwrap_or_else(|| ("en".into(), Vec::new()));
    Ok(Settings { default_language, languages, texts: rows.into_iter().map(|(l, k, t)| ((l, k), t)).collect() })
}

async fn load_and_cache(db: &PgPool, account_id: Uuid) -> sqlx::Result<Settings> {
    let mut conn = db.acquire().await?;
    let s = load(&mut conn, account_id).await?;
    if let Ok(mut c) = CACHE.lock() {
        if c.len() > 10_000 {
            c.clear();
        }
        c.insert(account_id, (Instant::now(), s.clone()));
    }
    Ok(s)
}

/// The account's settings from the cache. An entry older than [`CACHE_SECS`] is still used and
/// refreshed in the background, so sends never wait on it; only the first use on a server reads
/// it in line.
pub async fn settings(db: &PgPool, account_id: Uuid) -> sqlx::Result<Settings> {
    let cached = CACHE.lock().ok().and_then(|c| c.get(&account_id).cloned());
    match cached {
        Some((at, s)) => {
            if at.elapsed() >= Duration::from_secs(CACHE_SECS) && REFRESHING.lock().is_ok_and(|mut r| r.insert(account_id)) {
                let db = db.clone();
                tokio::spawn(async move {
                    if let Err(e) = load_and_cache(&db, account_id).await {
                        tracing::warn!(account = %account_id, error = %e, "couldn't refresh language settings");
                    }
                    if let Ok(mut r) = REFRESHING.lock() {
                        r.remove(&account_id);
                    }
                });
            }
            Ok(s)
        }
        None => load_and_cache(db, account_id).await,
    }
}

/// System texts in one language.
#[derive(Debug, Clone)]
pub struct Texts {
    pub lang: String,
    texts: HashMap<String, String>,
}

impl Texts {
    /// English as shipped.
    pub fn english() -> Self {
        Self { lang: "en".into(), texts: HashMap::new() }
    }

    /// `lang`'s texts for an account: its own wording, else the shipped text in that language,
    /// else English.
    pub fn for_lang(s: &Settings, lang: &str) -> Self {
        let mut texts = HashMap::new();
        for (k, _, _) in KEYS {
            if let Some(t) = s.texts.get(&(lang.to_string(), k.to_string())) {
                texts.insert(k.to_string(), t.clone());
            }
        }
        Self { lang: lang.to_string(), texts }
    }

    /// The text for `key`, with `{name}` (escape it first where it lands in HTML) and `{days}`
    /// filled in.
    pub fn get(&self, key: &str, name: &str) -> String {
        let t = self.texts.get(key).map(String::as_str).or_else(|| shipped_text(&self.lang, key)).or_else(|| shipped_text("en", key)).unwrap_or("");
        t.replace("{days}", &crate::optin::LINK_DAYS.to_string()).replace("{name}", name)
    }

    /// [`Texts::get`] for HTML: the text escaped, with `name` escaped too.
    pub fn html(&self, key: &str, name: &str) -> String {
        let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
        esc(&self.get(key, "\u{0}")).replace('\u{0}', &esc(name))
    }

    /// An HTML page in this language: `lang` and `dir` on its `<html>` tag (plain English pages are
    /// left as they are).
    pub fn page(&self, html: String) -> String {
        match self.html_attrs() {
            Some(a) => html.replacen(r#"<html lang="en">"#, &format!("<html {a}>"), 1),
            None => html,
        }
    }

    /// Whether this is plain English as shipped (then nothing about a page or email changes).
    pub fn is_plain_english(&self) -> bool {
        self.lang == "en" && self.texts.is_empty()
    }

    pub fn rtl(&self) -> bool {
        locale::is_rtl(&self.lang)
    }

    /// The `lang` (and `dir`) attributes for an `<html>` tag, or None for plain English.
    pub fn html_attrs(&self) -> Option<String> {
        if self.lang == "en" {
            return None;
        }
        Some(format!(r#"lang="{}"{}"#, self.lang, if self.rtl() { r#" dir="rtl""# } else { "" }))
    }
}

/// Which of the account's languages suits this person: their profile's, else the browser's, else
/// the default.
pub fn pick(s: &Settings, profile_locale: Option<&str>, accept: Option<&str>) -> String {
    let all = s.all();
    if let Some(m) = profile_locale.and_then(|l| locale::best_match(l, &all)) {
        return m.clone();
    }
    for want in accept.map(locale::accept_language).unwrap_or_default() {
        if let Some(m) = locale::best_match(&want, &all) {
            return m.clone();
        }
    }
    s.default_language.clone()
}

/// The texts for a page shown to the person behind `profile_id` (when known) in this browser.
/// Never fails: problems give English.
pub async fn for_page(db: &PgPool, account_id: Uuid, profile_id: Option<Uuid>, headers: &HeaderMap) -> Texts {
    let Ok(s) = settings(db, account_id).await else { return Texts::english() };
    let loc: Option<String> = match profile_id {
        Some(p) if !s.languages.is_empty() => sqlx::query_scalar("SELECT locale FROM profiles WHERE id = $1 AND account_id = $2").bind(p).bind(account_id).fetch_optional(db).await.ok().flatten().flatten(),
        _ => None,
    };
    let accept = headers.get(axum::http::header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok());
    Texts::for_lang(&s, &pick(&s, loc.as_deref(), accept))
}

/// The texts for an email to the person behind `profile_id` (the double opt-in email).
pub async fn for_profile(db: &PgPool, account_id: Uuid, profile_id: Uuid) -> Texts {
    for_page(db, account_id, Some(profile_id), &HeaderMap::new()).await
}

/// The footer lines for an email sent in `lang`, as the content prepare step stores them (`_footer`).
pub async fn footer(db: &PgPool, account_id: Uuid, lang: &str) -> Value {
    let s = settings(db, account_id).await.unwrap_or_default();
    footer_in(&s, lang)
}

/// The footer lines in `lang` from settings already read.
pub fn footer_in(s: &Settings, lang: &str) -> Value {
    let t = Texts::for_lang(s, lang);
    json!({"reason": t.get("footer.reason", ""), "unsubscribe": t.get("footer.unsubscribe", "")})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_language_has_every_text_with_its_placeholders() {
        for lang in SHIPPED {
            for (k, _, en) in KEYS {
                let t = shipped_text(lang, k).unwrap();
                assert!(!t.trim().is_empty(), "{lang} {k}");
                assert_eq!(placeholders(t), placeholders(en), "{lang} {k}");
            }
        }
    }

    #[test]
    fn english_is_unchanged() {
        let t = Texts::english();
        assert_eq!(t.get("unsub.email_question", "Acme"), "Stop receiving marketing emails from Acme?");
        assert_eq!(t.get("confirm.email_ignore", ""), "If you didn't sign up, ignore this email and you won't hear from us. The link works for 7 days.");
        assert!(t.html_attrs().is_none());
    }

    #[test]
    fn picks_profile_then_browser_then_default() {
        let s = Settings { default_language: "en".into(), languages: vec!["fr".into(), "ar".into()], texts: HashMap::new() };
        assert_eq!(pick(&s, Some("fr-CA"), Some("ar")), "fr");
        assert_eq!(pick(&s, None, Some("de, ar;q=0.5")), "ar");
        assert_eq!(pick(&s, Some("de"), None), "en");
        let ar = Texts::for_lang(&s, "ar");
        assert_eq!(ar.get("unsub.button", ""), "Unsubscribe", "no shipped Arabic: English until the account words it");
        assert_eq!(ar.html_attrs().as_deref(), Some(r#"lang="ar" dir="rtl""#));
        let mut s2 = s.clone();
        s2.texts.insert(("fr".into(), "unsub.button".into()), "Je me désabonne".into());
        assert_eq!(Texts::for_lang(&s2, "fr").get("unsub.button", ""), "Je me désabonne");
        assert_eq!(Texts::for_lang(&s2, "fr-CA").get("unsub.title", ""), "Se désabonner", "shipped by language part");
    }
}
