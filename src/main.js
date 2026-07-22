const { invoke } = window.__TAURI__.core;

window.addEventListener("DOMContentLoaded", () => {
  const updateStatusEl = document.querySelector("#update-status");
  const updateDetailsEl = document.querySelector("#update-details");
  const actionsEl = document.querySelector("#actions");
  const startServerMsgEl = document.querySelector("#start-server-msg");
  const publishButton = document.querySelector("#publish-server-btn");
  const publishMessage = document.querySelector("#publish-server-msg");
  const prepareButton = document.querySelector("#prepare-btn");
  const agreement = document.querySelector("#eula-agreement");
  agreement.addEventListener("change", () => { prepareButton.disabled = !agreement.checked; });
  prepareButton.addEventListener("click", async () => {
    prepareButton.disabled = true;
    updateStatusEl.textContent = "BDSとアドオンを準備しています…";
    updateDetailsEl.textContent = "初回はBDS本体をダウンロードするため数分かかります。";
    try {
      const result = await invoke("prepare_server");
      const updated = result.addons.filter((addon) => addon.updated).length;
      updateStatusEl.textContent = `準備完了（アドオン${updated}件更新）`;
      updateDetailsEl.textContent = result.addons
        .map((addon) => `${addon.addonId} ${addon.version}${addon.updated ? " — 更新済み" : ""}`)
        .concat([`BDS ${result.bds.version}${result.bds.updated ? " — 更新済み" : ""}`, `World: ${result.bds.worldName} / BP ${result.bds.behaviorPacks} / RP ${result.bds.resourcePacks}`])
        .join("\n");
      actionsEl.hidden = false;
    } catch (error) {
      updateStatusEl.textContent = "準備に失敗しました";
      updateDetailsEl.textContent = String(error);
      prepareButton.disabled = !agreement.checked;
    }
  });
  document.querySelector("#start-server-btn").addEventListener("click", async () => {
    try {
      const result = await invoke("start_server");
      startServerMsgEl.textContent = `BDSを起動しました（PID ${result.pid}）。接続先: ${result.address}:${result.port}`;
      publishButton.hidden = false;
    } catch (error) { startServerMsgEl.textContent = String(error); }
  });
  publishButton.addEventListener("click", async () => {
    publishButton.disabled = true;
    publishMessage.textContent = "Firewallとルーターを設定しています。Windowsの確認画面を許可してください…";
    try {
      const result = await invoke("publish_server");
      publishMessage.textContent = result.warning
        ? `中央サーバーへ登録しました（ID: ${result.serverId}）。${result.warning}`
        : `公開しました: ${result.publicAddress}（LAN: ${result.localAddress}）`;
    } catch (error) {
      publishMessage.textContent = `公開できませんでした: ${error}`;
      publishButton.disabled = false;
    }
  });
});
