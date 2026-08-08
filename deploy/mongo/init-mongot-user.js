// Create the user mongot authenticates as.
//
// Runs from `/docker-entrypoint-initdb.d` during first-boot initialisation,
// while the image has mongod up as a standalone with auth stripped. The write
// lands in the same dbPath the replica set later uses, so the user is there by
// the time mongot first connects — which matters, because mongot cannot be
// created afterwards without already being able to authenticate.
//
// `searchCoordinator` is the whole grant: mongot reads the oplog and manages
// search indexes, and nothing else.
// `fs`, not the legacy `cat()` helper: mongosh dropped it, and the failure is
// a ReferenceError at first boot rather than anything a log would explain.
const password = require("fs")
  .readFileSync("/run/secrets/mongo/mongot-password.initdb", "utf8")
  .trim();

db.getSiblingDB("admin").createUser({
  user: "mongotUser",
  pwd: password,
  roles: [{ role: "searchCoordinator", db: "admin" }],
});
