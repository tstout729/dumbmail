// Jotform → DumbMail
//
// Jotform's webhooks aren't signed. They post the submission as a multipart form (formID,
// submissionID, rawRequest with the answers, and more); DumbMail keeps only formID and
// submissionID. The integration's URL is the secret, and nothing in a webhook is trusted: the
// submission is fetched from Jotform's API with your API key (GET /submission/{id}), and only that
// answer is recorded. See README.md for the Jotform page behind each assumption.
//
// Form Submitted: once per submission, on the profile of the email answer (settings.email_field,
// the question's ID). A form submission isn't an opt-in: nobody is subscribed and nobody is
// renamed, and people this adds are held back from marketing. Only the form you choose
// (settings.form_id, required) is recorded, and only questions of safe types can be mapped: names,
// email, phone, short text, numbers, dates, dropdowns, radio buttons and checkboxes; never
// addresses, payments, signatures or file uploads. If the form has a consent question,
// map it (consent_field and the answer that means yes, consent_value): the answer is recorded on
// the event as marketing_consent, for you to follow up with a confirmation. Only the answers in
// copy_fields are copied onto the event (their values, not Jotform's formatted text); every other answer is dropped before anything is stored.

var ID = /^[0-9]{1,24}$/;
// Question types that may be mapped. Never addresses, payments, signatures, file uploads or
// anything else: those answers aren't kept, so they can't be copied.
var SAFE = ["control_fullname", "control_email", "control_phone", "control_textbox", "control_number", "control_datetime", "control_dropdown", "control_radio", "control_checkbox"];
var CONSENT_TYPES = ["control_checkbox", "control_radio", "control_dropdown"];

// A mapped answer's value: text, a number, a list of ticked options, or a name ("first last").
// Anything else (an answer given as some other object) isn't used.
function answerValue(a, types) {
  if (!a || typeof a !== "object" || (types || SAFE).indexOf(a.type) < 0) return undefined;
  var v = a.answer;
  if (typeof v === "string" || typeof v === "number" || typeof v === "boolean") return v;
  if (Array.isArray(v) && v.every(function (x) { return typeof x === "string"; })) return v;
  if (a.type === "control_fullname" && v && typeof v === "object") {
    var n = [v.first, v.last].filter(function (x) { return typeof x === "string" && x; }).join(" ");
    return n || undefined;
  }
  return undefined;
}

function fetched(input, id) {
  var s = input.settings || {};
  var body = input.payload || {};
  var c = body.content || {};
  if (String(c.id) !== id) return [{ type: "log", message: "the answer isn't submission " + id }];
  var form = String(c.form_id || "");
  if (!s.form_id || form !== String(s.form_id)) return [{ type: "log", message: "submission " + id + " isn't from the form this integration records (form_id)" }];
  if (!s.email_field) return [{ type: "log", message: "set email_field in the integration's settings to the ID of the form's email question" }];
  var answers = c.answers && typeof c.answers === "object" ? c.answers : {};
  var email = answerValue(answers[String(s.email_field)], ["control_email"]);
  if (typeof email !== "string" || email.indexOf("@") < 1) return [{ type: "log", message: "submission " + id + " has no email answer" }];
  var props = { form_id: form, submission_id: id };
  if (s.consent_field && s.consent_value) {
    var v = answerValue(answers[String(s.consent_field)], CONSENT_TYPES);
    var yes = String(s.consent_value);
    props.marketing_consent = v === yes || (Array.isArray(v) && v.indexOf(yes) >= 0);
  }
  var copy = Array.isArray(s.copy_fields) ? s.copy_fields : [];
  var copied = {};
  var n = 0;
  copy.forEach(function (q) {
    if (n >= 50) return;
    var v = answerValue(answers[String(q)]);
    if (v === undefined || v === "") return;
    copied[String(q)] = v;
    n++;
  });
  if (n) props.answers = copied;
  return [
    { type: "upsert_profile", email: email.trim() },
    { type: "track_event", metric: "Form Submitted", profile: { email: email.trim() }, unique_id: "jotform:" + form + ":" + id, properties: props },
  ];
}

function handle(input) {
  if (input.source === "pull") return [{ type: "log", message: "this recipe records Jotform's webhooks; it doesn't import history" }];
  if (input.source === "fetch") {
    var path = String((input.request && input.request.url) || "").replace(/^https?:\/\/[^\/]+/, "").split("?")[0];
    var m = /^\/submission\/([0-9]{1,24})$/.exec(path);
    return m ? fetched(input, m[1]) : [{ type: "log", message: "unexpected answer from " + path }];
  }
  var f = input.payload || {};
  var id = String(f.submissionID === undefined || f.submissionID === null ? "" : f.submissionID);
  var form = String(f.formID === undefined || f.formID === null ? "" : f.formID);
  if (!ID.test(id)) return [{ type: "log", message: "not a Jotform submission (no submissionID)" }];
  var s = input.settings || {};
  if (!s.form_id) return [{ type: "log", message: "set form_id in the integration's settings to the form to record" }];
  if (form !== String(s.form_id)) return [{ type: "log", message: "ignored a submission from another form (" + form + ")" }];
  return [{ type: "next_request", url: "/submission/" + id }];
}
