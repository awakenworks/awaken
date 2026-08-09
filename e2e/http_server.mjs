// Canonical shutdown boundary for E2E-owned HTTP fixtures.
//
// Cause/effect decision table:
// | listening | keep-alive sockets | effect |
// | no | any | already terminal; resolve without ERR_SERVER_NOT_RUNNING |
// | yes | no | stop accepting and resolve after close |
// | yes | yes | stop accepting, destroy retained sockets, then resolve |
// | yes | any close error | reject so cleanup failures remain observable |
//
// Fixture servers never own product data. Their shutdown must therefore be
// bounded by connection teardown rather than waiting forever for a product HTTP
// client's keep-alive timeout.
export function closeHttpServer(server) {
  if (!server.listening) {
    server.closeAllConnections?.();
    return Promise.resolve();
  }
  return new Promise((resolve, reject) => {
    server.close((error) => (error ? reject(error) : resolve()));
    server.closeAllConnections?.();
  });
}
