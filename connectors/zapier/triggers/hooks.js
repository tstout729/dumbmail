'use strict';

const { baseUrl } = require('../authentication');

// One REST hook trigger per DumbMail webhook topic. DumbMail sends one JSON envelope per event:
// {id, topic, created_at, account_id, api_version, data}. `id` stays the same on retries, which
// Zapier uses to skip duplicates.
const TOPICS = [
  { key: 'event_tracked', topic: 'event.tracked', noun: 'Event', label: 'New Event', description: 'Triggers when an event is tracked, such as Placed Order. Pick a metric to get only those.', metric: true },
  { key: 'profile_created', topic: 'profile.created', noun: 'Profile', label: 'New Profile', description: 'Triggers when a new profile is added.' },
  { key: 'profile_updated', topic: 'profile.updated', noun: 'Profile', label: 'Updated Profile', description: "Triggers when a profile's details, properties or consent change." },
  { key: 'profile_subscribed', topic: 'profile.subscribed', noun: 'Subscriber', label: 'New Subscriber', description: 'Triggers when someone subscribes to email or SMS.' },
  { key: 'profile_unsubscribed', topic: 'profile.unsubscribed', noun: 'Unsubscribe', label: 'New Unsubscribe', description: 'Triggers when someone unsubscribes from email or SMS.' },
  { key: 'list_member_added', topic: 'list.member_added', noun: 'List Member', label: 'Profile Added to List', description: 'Triggers when a profile joins a list.' },
  { key: 'segment_entered', topic: 'segment.entered', noun: 'Segment Member', label: 'Profile Entered Segment', description: 'Triggers when a profile enters a segment (checked about every 15 minutes).' },
  { key: 'form_submitted', topic: 'form.submitted', noun: 'Form Submission', label: 'New Form Submission', description: 'Triggers when someone signs up through a DumbMail form.' },
  { key: 'message_opened', topic: 'message.opened', noun: 'Open', label: 'Email Opened', description: 'Triggers when someone opens an email.' },
  { key: 'message_clicked', topic: 'message.clicked', noun: 'Click', label: 'Link Clicked', description: 'Triggers when someone clicks a link in an email or text.' },
  { key: 'message_bounced', topic: 'message.bounced', noun: 'Bounce', label: 'Email Bounced', description: 'Triggers when an email bounces.' },
  { key: 'campaign_finished', topic: 'campaign.finished', noun: 'Campaign', label: 'Campaign Sent', description: 'Triggers when a campaign has finished sending.' },
];

const makeTrigger = (t) => {
  const performSubscribe = async (z, bundle) => {
    const body = { url: bundle.targetUrl, topic: t.topic, source: 'zapier' };
    if (t.metric && bundle.inputData.metric) body.metric = bundle.inputData.metric;
    const response = await z.request({ url: `${baseUrl(bundle)}/api/hooks`, method: 'POST', body });
    // Stored by Zapier as bundle.subscribeData.
    return response.data;
  };

  const performUnsubscribe = async (z, bundle) => {
    const response = await z.request({ url: `${baseUrl(bundle)}/api/hooks/${bundle.subscribeData.id}`, method: 'DELETE', skipThrowForStatus: true });
    // Already gone is fine.
    if (response.status !== 404 && response.status >= 400) {
      throw new z.errors.Error(`Couldn't remove the DumbMail hook (HTTP ${response.status})`, 'UnsubscribeError', response.status);
    }
    return response.data || {};
  };

  const perform = async (z, bundle) => {
    const body = bundle.cleanedRequest;
    // A REST hook gets one envelope per request; accept an array too.
    return Array.isArray(body) ? body : [body];
  };

  const performList = async (z, bundle) => {
    const params = { topic: t.topic };
    if (t.metric && bundle.inputData.metric) params.metric = bundle.inputData.metric;
    const response = await z.request({ url: `${baseUrl(bundle)}/api/hooks/samples`, params });
    return response.data;
  };

  return {
    key: t.key,
    noun: t.noun,
    display: { label: t.label, description: t.description },
    operation: {
      type: 'hook',
      inputFields: t.metric
        ? [{ key: 'metric', label: 'Metric', required: false, helpText: 'Only trigger for this metric, e.g. `Placed Order`. Leave empty for every event.' }]
        : [],
      performSubscribe,
      performUnsubscribe,
      perform,
      performList,
      sample: {
        id: '0193a5b4-8c30-7a77-8e33-4b5c6d7e8f90',
        topic: t.topic,
        created_at: '2026-11-17T18:04:05Z',
        account_id: '0193a5b4-0000-7000-8000-000000000001',
        api_version: '2026-11-17',
        data: { profile: { id: '0193a5b4-6f1e-7c3a-9d2e-1b2c3d4e5f60', email: 'jane@example.com' } },
      },
      outputFields: [
        { key: 'id', label: 'Event ID' },
        { key: 'topic', label: 'Topic' },
        { key: 'created_at', label: 'Created At', type: 'datetime' },
        { key: 'data__profile__id', label: 'Profile ID' },
        { key: 'data__profile__email', label: 'Profile Email' },
      ],
    },
  };
};

module.exports = Object.fromEntries(TOPICS.map((t) => [t.key, makeTrigger(t)]));
module.exports.TOPICS = TOPICS;
