const { parentPort } = process;

function reply(value) {
  if (value === 'exit') process.exit(0);

  const message = {
    type: process.type ?? null,
    electron: process.versions.electron,
    modules: process.versions.modules,
    value
  };
  if (parentPort) {
    parentPort.postMessage(message);
  } else {
    process.send(message);
  }
}

if (parentPort) {
  parentPort.on('message', event => reply(event.data));
} else {
  process.on('message', reply);
}
