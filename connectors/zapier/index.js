'use strict';

const authentication = require('./authentication');
const hooks = require('./triggers/hooks');
const list = require('./triggers/lists');
const profile = require('./creates/profile');
const event = require('./creates/event');
const { addToList, removeFromList, unsubscribe } = require('./creates/lists');
const findProfile = require('./searches/profile');

const triggers = { [list.key]: list };
for (const t of hooks.TOPICS) triggers[t.key] = hooks[t.key];

module.exports = {
  version: require('./package.json').version,
  platformVersion: require('zapier-platform-core').version,
  authentication: authentication.config,
  flags: { cleanInputData: false },
  beforeRequest: [authentication.addAuthHeader],
  afterResponse: [authentication.handleErrors],
  triggers,
  creates: {
    [profile.key]: profile,
    [event.key]: event,
    [addToList.key]: addToList,
    [removeFromList.key]: removeFromList,
    [unsubscribe.key]: unsubscribe,
  },
  searches: { [findProfile.key]: findProfile },
  searchOrCreates: {
    [findProfile.key]: { key: findProfile.key, display: { label: 'Find or Create Profile', description: "Finds a profile, or creates it if it doesn't exist." }, search: findProfile.key, create: profile.key },
  },
};
