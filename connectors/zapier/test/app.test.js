'use strict';

const nock = require('nock');
const zapier = require('zapier-platform-core');
const App = require('../index');

const appTester = zapier.createAppTester(App);
zapier.tools.env.inject();

const BASE = 'https://dumbmail.example.com';
const authData = { api_key: 'pl_sk_test', base_url: `${BASE}/` };
const auth = { reqheaders: { authorization: 'Bearer pl_sk_test' } };

afterEach(() => nock.cleanAll());

describe('authentication', () => {
  it('tests the key and labels the connection with the account name', async () => {
    nock(BASE, auth).get('/api/hooks/me').reply(200, { account: { id: 'a1', name: 'Acme Goods' } });
    const res = await appTester(App.authentication.test, { authData });
    expect(res.account.name).toBe('Acme Goods');
    expect(App.authentication.connectionLabel({}, { inputData: res })).toBe('Acme Goods');
  });

  it('explains a refused key', async () => {
    nock(BASE).get('/api/hooks/me').reply(401, { error: { code: 'unauthorized', message: 'sign in required' } });
    await expect(appTester(App.authentication.test, { authData })).rejects.toThrow(/API key was refused/);
  });

  it('refuses a non-https address', async () => {
    await expect(appTester(App.authentication.test, { authData: { api_key: 'pl_sk_test', base_url: 'http://x.test' } })).rejects.toThrow(/https/);
  });
});

describe('REST hook trigger', () => {
  const trigger = App.triggers.event_tracked.operation;

  it('subscribes with the topic and metric, and unsubscribes by id', async () => {
    nock(BASE, auth)
      .post('/api/hooks', { url: 'https://hooks.zapier.com/abc', topic: 'event.tracked', source: 'zapier', metric: 'Placed Order' })
      .reply(201, { id: 'h1', url: 'https://hooks.zapier.com/abc', topic: 'event.tracked' });
    const sub = await appTester(trigger.performSubscribe, { authData, targetUrl: 'https://hooks.zapier.com/abc', inputData: { metric: 'Placed Order' } });
    expect(sub.id).toBe('h1');
    nock(BASE, auth).delete('/api/hooks/h1').reply(200, { ok: true });
    await appTester(trigger.performUnsubscribe, { authData, subscribeData: sub });
    // Already removed is fine.
    nock(BASE, auth).delete('/api/hooks/h1').reply(404, { error: { code: 'not_found', message: 'webhook endpoint not found' } });
    await appTester(trigger.performUnsubscribe, { authData, subscribeData: sub });
    expect(nock.isDone()).toBe(true);
  });

  it('turns a delivery into one item and lists samples', async () => {
    const env = { id: 'e1', topic: 'event.tracked', data: { event: { metric: 'Placed Order' } } };
    const items = await appTester(trigger.perform, { authData, cleanedRequest: env });
    expect(items).toEqual([env]);
    nock(BASE, auth).get('/api/hooks/samples').query({ topic: 'event.tracked', metric: 'Placed Order' }).reply(200, [env]);
    const samples = await appTester(trigger.performList, { authData, inputData: { metric: 'Placed Order' } });
    expect(samples[0].id).toBe('e1');
  });
});

describe('actions', () => {
  it('creates or updates a profile and adds it to a list', async () => {
    nock(BASE, auth)
      .post('/api/profiles', { email: 'jane@example.com', first_name: 'Jane', properties: { vip: 'yes' }, list_ids: ['l1'] })
      .reply(200, { id: 'p1', email: 'jane@example.com' });
    const res = await appTester(App.creates.profile.operation.perform, { authData, inputData: { email: 'jane@example.com', first_name: 'Jane', properties: { vip: 'yes' }, list_id: 'l1' } });
    expect(res.id).toBe('p1');
  });

  it('tracks an event with a value and unique id', async () => {
    nock(BASE, auth)
      .post('/api/events', { metric: 'Placed Order', profile: { email: 'jane@example.com' }, properties: {}, value: 42.5, unique_id: 'o-1' })
      .reply(200, { id: 'ev1', profile_id: 'p1' });
    const res = await appTester(App.creates.event.operation.perform, { authData, inputData: { metric: 'Placed Order', email: 'jane@example.com', value: '42.5', unique_id: 'o-1' } });
    expect(res.id).toBe('ev1');
  });

  it('removes a profile from a list by email', async () => {
    nock(BASE, auth).get('/api/profiles').query({ q: 'Jane@Example.com', limit: 10 }).reply(200, { data: [{ id: 'p2', email: 'janet@example.com' }, { id: 'p1', email: 'jane@example.com' }], total: 2 });
    nock(BASE, auth).delete('/api/lists/l1/members', { profile_ids: ['p1'] }).reply(200, { removed: 1 });
    const res = await appTester(App.creates.remove_from_list.operation.perform, { authData, inputData: { list_id: 'l1', email: 'Jane@Example.com' } });
    expect(res.removed).toBe(1);
  });

  it('unsubscribes from texts and shows API errors plainly', async () => {
    nock(BASE, auth).post('/api/profiles', { phone_number: '+15035550100', sms_consent: 'unsubscribed' }).reply(200, { id: 'p1', sms_consent: 'unsubscribed' });
    const res = await appTester(App.creates.unsubscribe.operation.perform, { authData, inputData: { channel: 'sms', phone_number: '+15035550100' } });
    expect(res.sms_consent).toBe('unsubscribed');
    nock(BASE, auth).post('/api/profiles').reply(400, { error: { code: 'bad_request', message: '"nope" is not a valid email' } });
    await expect(appTester(App.creates.unsubscribe.operation.perform, { authData, inputData: { channel: 'email', email: 'nope' } })).rejects.toThrow(/is not a valid email/);
  });
});
