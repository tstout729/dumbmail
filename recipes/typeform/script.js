// Typeform → DumbMail
//
// Each form_response webhook becomes Form Submitted on the respondent's profile (found by the
// form's email answer). A form submission isn't an opt-in: nobody is subscribed, and people this
// adds are held back from marketing. If the form has a consent question (a yes/no or legal
// question) and its ref is set as settings.consent_field, its answer is recorded on the event as
// marketing_consent, for you to follow up with a confirmation. Typeform respondents are anonymous
// web visitors, so a form never changes an existing profile's name.
// Free-text answers aren't copied; choice, yes/no, number and date answers are, by field ref.
// See README.md for the Typeform page behind each assumption.

var COPY = ["choice", "choices", "boolean", "number", "date"];

function answerValue(a) {
  if (a.type === "choice") return a.choice ? a.choice.label || a.choice.other : undefined;
  if (a.type === "choices") return a.choices ? (a.choices.labels || []).concat(a.choices.other ? [a.choices.other] : []) : undefined;
  return a[a.type];
}

function handle(input) {
  var p = input.payload || {};
  if (p.event_type !== "form_response" || !p.form_response) return [{ type: "log", message: "not a Typeform form_response" }];
  var r = p.form_response;
  var s = input.settings || {};
  var answers = r.answers || [];
  var email;
  for (var i = 0; i < answers.length; i++) {
    var a = answers[i];
    var ref = a.field && a.field.ref;
    if (a.type === "email" && (!s.email_field || ref === s.email_field)) { email = a.email; break; }
  }
  if (!email) return [{ type: "log", message: "response " + r.token + " has no email answer" }];
  var props = { form_id: r.form_id, form_title: r.definition && r.definition.title, response_id: r.token };
  var copied = {};
  var n = 0;
  answers.forEach(function (a) {
    var ref = a.field && a.field.ref;
    if (!ref || COPY.indexOf(a.type) < 0 || n >= 50) return;
    var v = answerValue(a);
    if (v === undefined) return;
    copied[ref] = v;
    n++;
    if (s.consent_field && ref === s.consent_field) props.marketing_consent = v === true;
  });
  if (s.consent_field && props.marketing_consent === undefined) props.marketing_consent = false;
  if (n) props.answers = copied;
  for (var k in props) if (props[k] === undefined || props[k] === null) delete props[k];
  return [
    { type: "upsert_profile", email: email },
    { type: "track_event", metric: "Form Submitted", profile: { email: email }, time: r.submitted_at, unique_id: "typeform:" + r.form_id + ":" + r.token, properties: props },
  ];
}
