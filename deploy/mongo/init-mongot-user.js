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
const password = cat("/run/secrets/mongo/mongot-password").trim();

db.getSiblingDB("admin").createUser({
  user: "mongotUser",
  pwd: password,
  roles: [{ role: "searchCoordinator", db: "admin" }],
});
