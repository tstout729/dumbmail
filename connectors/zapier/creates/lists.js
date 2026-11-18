'use strict';

const { baseUrl } = require('../authentication');

const findProfileId = async (z, bundle, email) => {
  const response = await z.request({ url: `${baseUrl(bundle)}/api/profiles`, params: { q: email, limit: 10 } });
  const match = response.data.data.find((p) => (p.email || '').toLowerCase() === email.trim().toLowerCase());
  return match ? match.id : null;
};

const addToList = {
  key: 'add_to_list',
  noun: 'List Member',
  display: { label: 'Add Profile to List', description: "Adds a person to a list by email, creating the profile if it's new." },
  operation: {
    inputFields: [
      { key: 'list_id', label: 'List', required: true, dynamic: 'list.id.name' },
      { key: 'email', label: 'Email', required: true },
    ],
    perform: async (z, bundle) => {
      const { list_id: listId, email } = bundle.inputData;
      const response = await z.request({ url: `${baseUrl(bundle)}/api/lists/${encodeURIComponent(listId)}/members`, method: 'POST', body: { emails: [email] } });
      return { list_id: listId, email, added: response.data.added };
    },
    sample: { list_id: '0193a5b4-9d40-7b88-9f44-5c6d7e8f9a01', email: 'jane@example.com', added: 1 },
  },
};

const removeFromList = {
  key: 'remove_from_list',
  noun: 'List Member',
  display: { label: 'Remove Profile From List', description: 'Removes a person from a list by email.' },
  operation: {
    inputFields: [
      { key: 'list_id', label: 'List', required: true, dynamic: 'list.id.name' },
      { key: 'email', label: 'Email', required: true },
    ],
    perform: async (z, bundle) => {
      const { list_id: listId, email } = bundle.inputData;
      const id = await findProfileId(z, bundle, email);
      if (!id) return { list_id: listId, email, removed: 0 };
      const response = await z.request({ url: `${baseUrl(bundle)}/api/lists/${encodeURIComponent(listId)}/members`, method: 'DELETE', body: { profile_ids: [id] } });
      return { list_id: listId, email, removed: response.data.removed };
    },
    sample: { list_id: '0193a5b4-9d40-7b88-9f44-5c6d7e8f9a01', email: 'jane@example.com', removed: 1 },
  },
};

const unsubscribe = {
  key: 'unsubscribe',
  noun: 'Unsubscribe',
  display: { label: 'Unsubscribe Profile', description: 'Unsubscribes a person from marketing email or texts.' },
  operation: {
    inputFields: [
      { key: 'channel', label: 'Channel', required: true, choices: { email: 'Email', sms: 'SMS' }, default: 'email' },
      { key: 'email', label: 'Email', required: false, helpText: 'Required for email.' },
      { key: 'phone_number', label: 'Phone Number', required: false, helpText: 'Required for SMS, e.g. +15035550100.' },
    ],
    perform: async (z, bundle) => {
      const d = bundle.inputData;
      const sms = d.channel === 'sms';
      if (sms ? !d.phone_number : !d.email) {
        throw new z.errors.Error(sms ? 'Give a phone number to unsubscribe from texts.' : 'Give an email to unsubscribe from email.', 'InvalidData', 400);
      }
      const body = sms ? { phone_number: d.phone_number, sms_consent: 'unsubscribed' } : { email: d.email, email_consent: 'unsubscribed' };
      const response = await z.request({ url: `${baseUrl(bundle)}/api/profiles`, method: 'POST', body });
      return response.data;
    },
    sample: { id: '0193a5b4-6f1e-7c3a-9d2e-1b2c3d4e5f60', email: 'jane@example.com', email_consent: 'unsubscribed', sms_consent: 'never_subscribed' },
  },
};

module.exports = { addToList, removeFromList, unsubscribe, findProfileId };
