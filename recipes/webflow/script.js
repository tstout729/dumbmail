// Webflow forms → DumbMail
//
// Each form_submission webhook becomes Form Submitted on the visitor's profile, found by the
// submission's email field (settings.email_field, or the first field named like "email"). A form
// submission isn't an opt-in: nobody is subscribed, and people this adds are held back from
// marketing. If the form has a consent checkbox and its field name is set as
// settings.consent_field, whether it was ticked is recorded on the event as marketing_consent.
// Visitors are anonymous, so a submission never changes an existing profile's name.
// Only the fields named in settings.copy_fields are copied onto the event.
// See README.md for the Webflow page behind each assumption.

function ticked(v) {
  if (v === true) return true;
  var s = String(v === undefined || v === null ? "" : v).toLowerCase();
  return s === "true" || s === "on" || s === "yes" || s === "checked";
}

function handle(input) {
  var p = input.payload || {};
  if (p.triggerType !== "form_submission" || !p.payload) return [{ type: "log", message: "not a Webflow form_submission" }];
  var f = p.payload;
  var data = f.data || {};
  var s = input.settings || {};
  var email = s.email_field ? data[s.email_field] : undefined;
  if (!email) {
    for (var k in data) {
      if (/e-?mail/i.test(k) && /@/.test(String(data[k]))) { email = data[k]; break; }
    }
  }
  if (!email) return [{ type: "log", message: "submission " + f.id + " has no email field" }];
  var props = { form_id: f.formId, form_name: f.name, site_id: f.siteId, submission_id: f.id };
  if (s.consent_field) props.marketing_consent = ticked(data[s.consent_field]);
  var copy = Array.isArray(s.copy_fields) ? s.copy_fields.slice(0, 50) : [];
  var fields = {};
  copy.forEach(function (name) {
    if (data[name] !== undefined && data[name] !== null) fields[name] = data[name];
  });
  if (Object.keys(fields).length) props.fields = fields;
  for (var j in props) if (props[j] === undefined || props[j] === null) delete props[j];
  return [
    { type: "upsert_profile", email: String(email).trim() },
    { type: "track_event", metric: "Form Submitted", profile: { email: String(email).trim() }, time: f.submittedAt, unique_id: "webflow:" + f.id, properties: props },
  ];
}
