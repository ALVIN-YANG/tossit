<script lang="ts">
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { appCacheDir, BaseDirectory, join } from "@tauri-apps/api/path";
  import { open } from "@tauri-apps/plugin-dialog";
  import { copyFile, mkdir } from "@tauri-apps/plugin-fs";
  import { openPath } from "@tauri-apps/plugin-opener";
  import { onMount, tick } from "svelte";

  type DeviceIdentity = {
    peerId: string;
    displayId: string;
    publicKey: string;
    nickname: string;
    avatarHash: string | null;
    avatarPath: string | null;
  };

  type NearbyPeer = {
    peerId: string;
    displayId: string;
    alias: string;
    endpoint: string | null;
    isOnline: boolean;
    lastSeenUnixMs: number;
    trustState: "discovered" | "trusted" | "blocked";
    verificationCode: string;
    unreadCount: number;
    avatarHash: string | null;
    avatarPath: string | null;
  };

  type AttachmentKind = "image" | "file";

  type ChatAttachment = {
    transferId: string;
    kind: AttachmentKind;
    fileName: string;
    mediaType: string;
    byteSize: number;
    transferredBytes: number;
    localPath: string | null;
    previewPath: string | null;
  };

  type ChatContent =
    | { type: "text"; text: string }
    | { type: "attachment"; attachment: ChatAttachment };

  type ChatMessage = {
    messageId: string;
    conversationId: string;
    networkId: string;
    peerId: string;
    direction: "incoming" | "outgoing";
    delivery: "received" | "receiving" | "sending" | "delivered" | "failed";
    content: ChatContent;
    createdAtUnixMs: number;
    isRead: boolean;
  };

  type ActiveNetwork = {
    networkId: string;
    displayName: string;
  };

  type NetworkSpace = ActiveNetwork & {
    firstUsedUnixMs: number;
    lastUsedUnixMs: number;
  };

  type AppleConnectivity = {
    kind: "wifi" | "localNetwork" | "cellular" | "offline" | "unsupported";
    permission: "prompt" | "granted" | "limited" | "denied" | "restricted" | "unsupported";
    ssid: string | null;
    networkId: string | null;
    canMessage: boolean;
  };

  type NetworkSnapshot = {
    listeningPort: number;
    localEndpoints: string[];
    activeNetwork: ActiveNetwork | null;
    networkSpaces: NetworkSpace[];
    peers: NearbyPeer[];
    messages: ChatMessage[];
  };

  type HistoryPage = {
    snapshot: NetworkSnapshot;
    loaded: number;
    hasMore: boolean;
  };

  type StorageSummary = {
    receivedFileCount: number;
    receivedBytes: number;
  };

  type MessageMenu = {
    messageId: string;
    x: number;
    y: number;
  };

  let identity = $state<DeviceIdentity | null>(null);
  let network = $state<NetworkSnapshot>({
    listeningPort: 0,
    localEndpoints: [],
    activeNetwork: null,
    networkSpaces: [],
    peers: [],
    messages: [],
  });
  let connectivity = $state<AppleConnectivity>({
    kind: "offline",
    permission: "prompt",
    ssid: null,
    networkId: null,
    canMessage: false,
  });
  let selectedNetworkId = $state("");
  let selectedPeerId = $state("");
  let draft = $state("");
  let sendError = $state("");
  let sending = $state(false);
  let picking = $state(false);
  let updatingTrust = $state(false);
  let manualConnectOpen = $state(false);
  let manualEndpoint = $state("");
  let manualConnectError = $state("");
  let connectingEndpoint = $state(false);
  let activeImage = $state<ChatAttachment | null>(null);
  let previewMode = $state(false);
  let requestingNetworkAccess = $state(false);
  let networkAccessError = $state("");
  let nicknameEditorOpen = $state(false);
  let nicknameDraft = $state("");
  let nicknameSaving = $state(false);
  let nicknameError = $state("");
  let avatarSaving = $state(false);
  let historyLoading = $state(false);
  let historyHasMore = $state<Record<string, boolean>>({});
  let storageSummary = $state<StorageSummary | null>(null);
  let clearingStorage = $state(false);
  let messageScrollElement = $state<HTMLDivElement | null>(null);
  let messageMenu = $state<MessageMenu | null>(null);
  let deletingMessage = $state(false);
  let messageLongPressTimer: number | null = null;
  let messageLongPressStart: { pointerId: number; messageId: string; x: number; y: number } | null = null;
  let messageLongPressTriggered = "";
  const syncingAvatars = new Set<string>();
  const avatarRetryAfter = new Map<string, number>();

  const previewImage = `data:image/svg+xml,${encodeURIComponent(`
    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 800">
      <rect width="1200" height="800" fill="#e8eefc"/>
      <circle cx="920" cy="190" r="92" fill="#ffcf66"/>
      <path d="M0 650 330 315l190 215 170-155 510 425H0Z" fill="#7797eb"/>
      <path d="M0 720 350 460l205 175 190-120 455 285H0Z" fill="#275dff"/>
      <circle cx="340" cy="285" r="72" fill="#fff" opacity=".78"/>
    </svg>
  `)}`;

  const previewAvatar = `data:image/svg+xml,${encodeURIComponent(`
    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256">
      <defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop stop-color="#8cb4ff"/><stop offset="1" stop-color="#315fe9"/></linearGradient></defs>
      <rect width="256" height="256" rx="54" fill="url(#g)"/>
      <circle cx="128" cy="98" r="45" fill="#fff" opacity=".92"/>
      <path d="M42 240c9-57 39-86 86-86s77 29 86 86" fill="#fff" opacity=".92"/>
    </svg>
  `)}`;

  const previewSnapshot: NetworkSnapshot = {
    listeningPort: 42318,
    localEndpoints: ["192.168.10.176:42318"],
    activeNetwork: { networkId: "home", displayName: "家里的 Wi-Fi" },
    networkSpaces: [
      {
        networkId: "home",
        displayName: "家里的 Wi-Fi",
        firstUsedUnixMs: Date.now() - 86_400_000 * 12,
        lastUsedUnixMs: Date.now() - 32_000,
      },
      {
        networkId: "office",
        displayName: "工作室",
        firstUsedUnixMs: Date.now() - 86_400_000 * 30,
        lastUsedUnixMs: Date.now() - 86_400_000,
      },
    ],
    peers: [
      {
        peerId: "62B9A13350BE7E474A41B6C43105C4A42CD91C441599D223106385A901C17E29",
        displayId: "62B9-A133-50BE",
        alias: "Yang 的 iPhone",
        endpoint: "192.168.10.24:42318",
        isOnline: true,
        lastSeenUnixMs: Date.now(),
        trustState: "trusted",
        verificationCode: "428 615",
        unreadCount: 0,
        avatarHash: "preview-avatar",
        avatarPath: previewAvatar,
      },
      {
        peerId: "97A30C51A7720511B0C51110FE6A03B9AD07C225112A48512892CC8C1FA12219",
        displayId: "97A3-0C51-A772",
        alias: "工作室的 Mac",
        endpoint: "192.168.10.25:42318",
        isOnline: true,
        lastSeenUnixMs: Date.now(),
        trustState: "discovered",
        verificationCode: "731 204",
        unreadCount: 0,
        avatarHash: null,
        avatarPath: null,
      },
    ],
    messages: [
      {
        messageId: "preview-1",
        conversationId: "preview",
        networkId: "home",
        peerId: "62B9A13350BE7E474A41B6C43105C4A42CD91C441599D223106385A901C17E29",
        direction: "incoming",
        delivery: "received",
        content: { type: "text", text: "这条消息只在当前局域网里传输。" },
        createdAtUnixMs: Date.now() - 75_000,
        isRead: true,
      },
      {
        messageId: "preview-image",
        conversationId: "preview",
        networkId: "home",
        peerId: "62B9A13350BE7E474A41B6C43105C4A42CD91C441599D223106385A901C17E29",
        direction: "incoming",
        delivery: "received",
        content: {
          type: "attachment",
          attachment: {
            transferId: "preview-transfer",
            kind: "image",
            fileName: "周末路线.png",
            mediaType: "image/png",
            byteSize: 1_842_300,
            transferredBytes: 1_842_300,
            localPath: previewImage,
            previewPath: previewImage,
          },
        },
        createdAtUnixMs: Date.now() - 45_000,
        isRead: true,
      },
      {
        messageId: "preview-2",
        conversationId: "preview",
        networkId: "home",
        peerId: "62B9A13350BE7E474A41B6C43105C4A42CD91C441599D223106385A901C17E29",
        direction: "outgoing",
        delivery: "delivered",
        content: {
          type: "attachment",
          attachment: {
            transferId: "preview-file",
            kind: "file",
            fileName: "露营清单.pdf",
            mediaType: "application/pdf",
            byteSize: 684_200,
            transferredBytes: 684_200,
            localPath: null,
            previewPath: null,
          },
        },
        createdAtUnixMs: Date.now() - 32_000,
        isRead: true,
      },
      {
        messageId: "preview-office",
        conversationId: "preview-office",
        networkId: "office",
        peerId: "97A30C51A7720511B0C51110FE6A03B9AD07C225112A48512892CC8C1FA12219",
        direction: "incoming",
        delivery: "received",
        content: { type: "text", text: "工作室里的对话会留在这个网络空间。" },
        createdAtUnixMs: Date.now() - 86_400_000,
        isRead: false,
      },
    ],
  };

  const previewConnectivity: AppleConnectivity = {
    kind: "wifi",
    permission: "granted",
    ssid: "家里的 Wi-Fi",
    networkId: "home",
    canMessage: true,
  };

  function currentPeer(): NearbyPeer | undefined {
    return network.peers.find((peer) => peer.peerId === selectedPeerId);
  }

  function selectedNetwork(): ActiveNetwork | NetworkSpace | undefined {
    if (network.activeNetwork?.networkId === selectedNetworkId) return network.activeNetwork;
    return network.networkSpaces.find((space) => space.networkId === selectedNetworkId);
  }

  function selectableNetworks(): (ActiveNetwork | NetworkSpace)[] {
    const choices: (ActiveNetwork | NetworkSpace)[] = [];
    if (network.activeNetwork) choices.push(network.activeNetwork);
    choices.push(
      ...network.networkSpaces
        .filter((space) => space.networkId !== network.activeNetwork?.networkId)
        .sort((left, right) => right.lastUsedUnixMs - left.lastUsedUnixMs),
    );
    return choices;
  }

  function defaultNetworkId(snapshot: NetworkSnapshot): string {
    if (snapshot.activeNetwork) return snapshot.activeNetwork.networkId;
    return [...snapshot.networkSpaces].sort(
      (left, right) => right.lastUsedUnixMs - left.lastUsedUnixMs,
    )[0]?.networkId ?? "";
  }

  function isSelectedNetworkCurrent(): boolean {
    return Boolean(selectedNetworkId && network.activeNetwork?.networkId === selectedNetworkId && connectivity.canMessage);
  }

  function isPersistedNetwork(networkId: string): boolean {
    return network.networkSpaces.some((space) => space.networkId === networkId);
  }

  function messagesFor(peerId: string, networkId = selectedNetworkId): ChatMessage[] {
    return network.messages
      .filter((message) => message.peerId === peerId && message.networkId === networkId)
      .sort((left, right) => left.createdAtUnixMs - right.createdAtUnixMs);
  }

  function conversationKey(peerId = selectedPeerId, networkId = selectedNetworkId): string {
    return `${networkId}:${peerId}`;
  }

  function lastMessage(peerId: string, networkId = selectedNetworkId): ChatMessage | undefined {
    return messagesFor(peerId, networkId).at(-1);
  }

  function networkMessages(networkId: string): ChatMessage[] {
    return network.messages
      .filter((message) => message.networkId === networkId)
      .sort((left, right) => left.createdAtUnixMs - right.createdAtUnixMs);
  }

  function peerUnread(peerId: string): number {
    return messagesFor(peerId).filter((message) => message.direction === "incoming" && !message.isRead).length;
  }

  function conversationPeers(): NearbyPeer[] {
    const peerIds = new Set(networkMessages(selectedNetworkId).map((message) => message.peerId));
    return network.peers.filter((peer) => peerIds.has(peer.peerId));
  }

  function nearbyPeers(): NearbyPeer[] {
    if (!isSelectedNetworkCurrent()) return [];
    const existing = new Set(conversationPeers().map((peer) => peer.peerId));
    return network.peers.filter((peer) => peer.isOnline && !existing.has(peer.peerId));
  }

  function peerName(peer: NearbyPeer): string {
    return peer.alias.trim() || `TossIt ${peer.displayId}`;
  }

  function openNicknameEditor() {
    if (!identity) return;
    nicknameDraft = identity.nickname;
    nicknameError = "";
    nicknameEditorOpen = true;
    if (!previewMode) void refreshStorageSummary();
  }

  async function refreshStorageSummary() {
    try {
      storageSummary = await invoke<StorageSummary>("storage_summary");
    } catch (error) {
      nicknameError = error instanceof Error ? error.message : String(error);
    }
  }

  async function clearReceivedFiles() {
    if (
      previewMode ||
      clearingStorage ||
      !storageSummary?.receivedFileCount ||
      !window.confirm("删除本机已接收的图片和文件？聊天记录会保留。")
    ) return;
    clearingStorage = true;
    nicknameError = "";
    try {
      storageSummary = await invoke<StorageSummary>("clear_received_files");
      await refreshNetwork();
    } catch (error) {
      nicknameError = error instanceof Error ? error.message : String(error);
    } finally {
      clearingStorage = false;
    }
  }

  async function saveNickname(event: SubmitEvent) {
    event.preventDefault();
    const nickname = nicknameDraft.trim();
    if (!identity || !nickname || nicknameSaving || avatarSaving) return;
    nicknameSaving = true;
    nicknameError = "";
    try {
      if (previewMode) {
        identity = { ...identity, nickname };
      } else {
        identity = await invoke<DeviceIdentity>("set_device_nickname", { nickname });
        await refreshNetwork();
      }
      nicknameEditorOpen = false;
    } catch (error) {
      nicknameError = error instanceof Error ? error.message : String(error);
    } finally {
      nicknameSaving = false;
    }
  }

  async function chooseAvatar() {
    if (avatarSaving) return;
    nicknameError = "";
    if (previewMode) {
      if (identity) identity = { ...identity, avatarHash: "preview-avatar", avatarPath: previewAvatar };
      return;
    }
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        filters: [{ name: "图片", extensions: ["jpg", "jpeg", "png", "webp", "bmp", "gif"] }],
      });
      if (typeof selected !== "string") return;
      avatarSaving = true;
      const normalized = await normalizeSelectedPath(selected, "image");
      identity = await invoke<DeviceIdentity>("set_device_avatar", { path: normalized.path });
    } catch (error) {
      nicknameError = error instanceof Error ? error.message : String(error);
    } finally {
      avatarSaving = false;
    }
  }

  async function removeAvatar() {
    if (!identity?.avatarPath || avatarSaving) return;
    nicknameError = "";
    avatarSaving = true;
    try {
      if (previewMode) {
        identity = { ...identity, avatarHash: null, avatarPath: null };
      } else {
        identity = await invoke<DeviceIdentity>("remove_device_avatar");
      }
    } catch (error) {
      nicknameError = error instanceof Error ? error.message : String(error);
    } finally {
      avatarSaving = false;
    }
  }

  function selectNetwork(networkId: string) {
    selectedNetworkId = networkId;
    selectedPeerId = "";
    sendError = "";
    manualConnectOpen = false;
  }

  function handleNetworkSelect(event: Event) {
    const networkId = (event.currentTarget as HTMLSelectElement).value;
    if (networkId && networkId !== selectedNetworkId) selectNetwork(networkId);
  }

  function messagePreview(message: ChatMessage | undefined): string {
    if (!message) return "";
    if (message.content.type === "text") return message.content.text;
    const attachment = message.content.attachment;
    return `${attachment.kind === "image" ? "[图片]" : "[文件]"} ${attachment.fileName}`;
  }

  function formatTime(timestamp: number): string {
    return new Intl.DateTimeFormat("zh-CN", {
      hour: "2-digit",
      minute: "2-digit",
      hour12: false,
    }).format(timestamp);
  }

  function connectivityTitle(): string {
    if (connectivity.kind === "wifi" && network.activeNetwork) return network.activeNetwork.displayName;
    if (connectivity.kind === "cellular") return "正在使用蜂窝网络";
    if (connectivity.kind === "offline") return "当前没有网络";
    if (connectivity.permission === "denied" || connectivity.permission === "restricted") return "无法识别当前 Wi-Fi";
    return "识别当前 Wi-Fi";
  }

  function connectivityDescription(): string {
    if (connectivity.kind === "wifi" && network.activeNetwork) {
      return isPersistedNetwork(network.activeNetwork.networkId)
        ? "当前可发现设备并继续对话"
        : "发出或收到第一条内容后保存";
    }
    if (connectivity.kind === "cellular") return "连接 Wi-Fi 后继续发送";
    if (connectivity.kind === "offline") return "连接 Wi-Fi 后继续发送";
    if (connectivity.permission === "denied" || connectivity.permission === "restricted") {
      return "可在系统设置中允许定位";
    }
    return "只用于区分当前网络，不会读取位置";
  }

  function canSendTo(peer: NearbyPeer): boolean {
    return isSelectedNetworkCurrent() && peer.trustState === "trusted" && !previewMode;
  }

  function composerPlaceholder(peer: NearbyPeer): string {
    if (!isSelectedNetworkCurrent()) return `连接“${selectedNetwork()?.displayName ?? "这个 Wi-Fi"}”后可发送`;
    if (peer.trustState !== "trusted") return "确认设备后可发送";
    return peer.isOnline ? "发一条局域网消息…" : "发出后等待对方上线…";
  }

  function composerHint(peer: NearbyPeer): string {
    if (!isSelectedNetworkCurrent()) return "切回这个 Wi-Fi 后即可继续发送";
    if (peer.trustState !== "trusted") return "先确认设备";
    if (previewMode) return "浏览器中只展示界面；打开 TossIt 应用后可发送";
    if (picking) return "正在处理附件…";
    if (!peer.isOnline) return "对方上线后自动发送";
    return "单个文件最大 512 MB · Enter 发送";
  }

  async function scrollConversationToBottom() {
    await tick();
    window.requestAnimationFrame(() => {
      if (messageScrollElement) messageScrollElement.scrollTop = messageScrollElement.scrollHeight;
    });
  }

  async function selectPeer(peerId: string) {
    selectedPeerId = peerId;
    sendError = "";
    void scrollConversationToBottom();
    if (previewMode) return;
    if (peerUnread(peerId) > 0) {
      try {
        network = await invoke<NetworkSnapshot>("mark_peer_read", { peerId, networkId: selectedNetworkId });
      } catch (error) {
        sendError = error instanceof Error ? error.message : String(error);
      }
    }
  }

  async function loadOlderMessages() {
    const messages = messagesFor(selectedPeerId);
    const oldest = messages[0];
    if (!oldest || historyLoading || previewMode) return;
    const key = conversationKey();
    const previousHeight = messageScrollElement?.scrollHeight ?? 0;
    historyLoading = true;
    sendError = "";
    try {
      const page = await invoke<HistoryPage>("load_older_messages", {
        networkId: selectedNetworkId,
        peerId: selectedPeerId,
        beforeCreatedAtUnixMs: oldest.createdAtUnixMs,
        beforeMessageId: oldest.messageId,
        limit: 50,
      });
      network = page.snapshot;
      historyHasMore = { ...historyHasMore, [key]: page.hasMore };
      await tick();
      if (messageScrollElement) {
        messageScrollElement.scrollTop += messageScrollElement.scrollHeight - previousHeight;
      }
    } catch (error) {
      sendError = error instanceof Error ? error.message : String(error);
    } finally {
      historyLoading = false;
    }
  }

  async function updatePeerTrust(peerId: string, trusted: boolean) {
    if (previewMode || updatingTrust) return;
    updatingTrust = true;
    sendError = "";
    try {
      network = await invoke<NetworkSnapshot>(trusted ? "trust_peer" : "block_peer", { peerId });
      scheduleAvatarSync(network);
    } catch (error) {
      sendError = error instanceof Error ? error.message : String(error);
    } finally {
      updatingTrust = false;
    }
  }

  async function connectManualEndpoint(event: SubmitEvent) {
    event.preventDefault();
    const endpoint = manualEndpoint.trim();
    if (!endpoint || connectingEndpoint || !isSelectedNetworkCurrent()) return;
    if (previewMode) {
      manualConnectError = "请在 TossIt 应用中连接设备";
      return;
    }
    connectingEndpoint = true;
    manualConnectError = "";
    const knownPeers = new Set(network.peers.map((peer) => peer.peerId));
    try {
      const snapshot = await invoke<NetworkSnapshot>("connect_endpoint", { endpoint });
      network = snapshot;
      scheduleAvatarSync(snapshot);
      const connected =
        snapshot.peers.find((peer) => !knownPeers.has(peer.peerId)) ??
        snapshot.peers.find((peer) => peer.endpoint === endpoint) ??
        snapshot.peers.find((peer) => peer.isOnline);
      if (connected) {
        selectedPeerId = connected.peerId;
        void scrollConversationToBottom();
      }
      manualEndpoint = "";
      manualConnectOpen = false;
    } catch (error) {
      manualConnectError = error instanceof Error ? error.message : String(error);
    } finally {
      connectingEndpoint = false;
    }
  }

  function toggleManualConnect() {
    manualConnectOpen = !manualConnectOpen;
    manualConnectError = "";
  }

  function formatBytes(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(bytes < 10 * 1024 ? 1 : 0)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(bytes < 10 * 1024 * 1024 ? 1 : 0)} MB`;
  }

  function transferPercent(attachment: ChatAttachment): number {
    if (attachment.byteSize === 0) return 100;
    return Math.min(100, Math.round((attachment.transferredBytes / attachment.byteSize) * 100));
  }

  function deliveryText(message: ChatMessage): string {
    if (message.delivery === "sending") return currentPeer()?.isOnline ? "发送中" : "等待上线";
    if (message.delivery === "receiving") return "接收中";
    if (message.delivery === "delivered") return "已送达";
    if (message.delivery === "failed") return message.direction === "outgoing" ? "发送失败" : "接收失败";
    return "已接收";
  }

  function assetUrl(path: string | null): string {
    if (!path) return "";
    return path.startsWith("data:") ? path : convertFileSrc(path);
  }

  function selectedFileName(path: string, kind: AttachmentKind): string {
    const withoutQuery = path.split(/[?#]/, 1)[0];
    const raw = withoutQuery.split("/").at(-1) ?? "";
    let decoded = raw;
    try {
      decoded = decodeURIComponent(raw);
    } catch {
      // Keep the original last path component when a provider returns malformed escaping.
    }
    const safe = decoded.replace(/[\\/\u0000-\u001f\u007f]/g, "_").slice(0, 180);
    return safe && safe !== "." && safe !== ".."
      ? safe
      : `${kind === "image" ? "图片" : "文件"}-${Date.now()}`;
  }

  async function normalizeSelectedPath(
    selected: string,
    kind: AttachmentKind,
  ): Promise<{ path: string; fileName: string }> {
    const fileName = selectedFileName(selected, kind);
    if (!selected.startsWith("content://")) return { path: selected, fileName };

    await mkdir("tossit-selected", {
      baseDir: BaseDirectory.AppCache,
      recursive: true,
    });
    const destination = await join(
      await appCacheDir(),
      "tossit-selected",
      `${crypto.randomUUID()}-${fileName}`,
    );
    await copyFile(selected, destination);
    return { path: destination, fileName };
  }

  async function refreshNetwork() {
    let snapshot = await invoke<NetworkSnapshot>("network_snapshot");
    const selectedHasUnread = snapshot.messages.some(
      (message) =>
        message.networkId === selectedNetworkId &&
        message.peerId === selectedPeerId &&
        message.direction === "incoming" &&
        !message.isRead,
    );
    if (selectedPeerId && selectedNetworkId && selectedHasUnread) {
      snapshot = await invoke<NetworkSnapshot>("mark_peer_read", {
        peerId: selectedPeerId,
        networkId: selectedNetworkId,
      });
    }
    network = snapshot;
    scheduleAvatarSync(snapshot);
  }

  function scheduleAvatarSync(snapshot: NetworkSnapshot) {
    if (previewMode) return;
    const now = Date.now();
    for (const peer of snapshot.peers) {
      if (
        !peer.isOnline ||
        peer.trustState !== "trusted" ||
        !peer.avatarHash ||
        peer.avatarPath ||
        syncingAvatars.has(peer.peerId) ||
        (avatarRetryAfter.get(peer.peerId) ?? 0) > now
      ) continue;
      syncingAvatars.add(peer.peerId);
      void invoke<NetworkSnapshot>("sync_peer_avatar", { peerId: peer.peerId })
        .then((next) => {
          network = next;
          avatarRetryAfter.delete(peer.peerId);
        })
        .catch(() => {
          avatarRetryAfter.set(peer.peerId, Date.now() + 10_000);
        })
        .finally(() => syncingAvatars.delete(peer.peerId));
    }
  }

  async function refreshConnectivity() {
    const previousActiveNetworkId = network.activeNetwork?.networkId ?? null;
    const followedActiveNetwork = !selectedNetworkId || selectedNetworkId === previousActiveNetworkId;
    try {
      const next = await invoke<AppleConnectivity>("current_connectivity");
      connectivity = next;
      network.activeNetwork = next.networkId && next.ssid
        ? { networkId: next.networkId, displayName: next.ssid }
        : null;
      networkAccessError = "";
      if (next.networkId && followedActiveNetwork && selectedNetworkId !== next.networkId) {
        selectNetwork(next.networkId);
      } else if (
        selectedNetworkId &&
        selectedNetworkId === previousActiveNetworkId &&
        selectedNetworkId !== next.networkId &&
        !isPersistedNetwork(selectedNetworkId)
      ) {
        selectNetwork(defaultNetworkId(network));
      } else if (!selectedNetworkId) {
        selectedNetworkId = defaultNetworkId(network);
      }
    } catch (error) {
      connectivity = {
        kind: "offline",
        permission: connectivity.permission,
        ssid: null,
        networkId: null,
        canMessage: false,
      };
      network.activeNetwork = null;
      networkAccessError = error instanceof Error ? error.message : String(error);
    }
  }

  async function requestNetworkAccess() {
    if (previewMode || requestingNetworkAccess) return;
    requestingNetworkAccess = true;
    networkAccessError = "";
    try {
      await invoke("request_network_access");
      window.setTimeout(() => refreshConnectivity().catch(() => undefined), 500);
    } catch (error) {
      networkAccessError = error instanceof Error ? error.message : String(error);
    } finally {
      requestingNetworkAccess = false;
    }
  }

  async function submitMessage(event: SubmitEvent) {
    event.preventDefault();
    const peer = currentPeer();
    const text = draft.trim();
    if (!peer || !canSendTo(peer) || !text || sending) return;

    sending = true;
    sendError = "";
    try {
      await invoke<ChatMessage>("send_text", {
        peerId: peer.peerId,
        text,
      });
      draft = "";
      await refreshNetwork();
    } catch (error) {
      sendError = error instanceof Error ? error.message : String(error);
      await refreshNetwork().catch(() => undefined);
    } finally {
      sending = false;
    }
  }

  async function pickAttachment(kind: AttachmentKind) {
    const peer = currentPeer();
    if (!peer || !canSendTo(peer) || picking || sending) return;

    picking = true;
    sendError = "";
    try {
      const selected = await open({
        title: kind === "image" ? "选择图片" : "选择文件",
        multiple: false,
        directory: false,
        pickerMode: kind === "image" ? "image" : "document",
        fileAccessMode: "copy",
        filters:
          kind === "image"
            ? [{ name: "图片", extensions: ["jpg", "jpeg", "png", "webp", "gif", "bmp"] }]
            : undefined,
      });
      if (!selected || Array.isArray(selected)) return;
      const normalized = await normalizeSelectedPath(selected, kind);
      await invoke<ChatMessage>("send_attachment", {
        peerId: peer.peerId,
        path: normalized.path,
        kind,
        fileName: normalized.fileName,
      });
      await refreshNetwork();
    } catch (error) {
      sendError = error instanceof Error ? error.message : String(error);
      await refreshNetwork().catch(() => undefined);
    } finally {
      picking = false;
    }
  }

  async function openFile(attachment: ChatAttachment) {
    if (!attachment.localPath || previewMode) return;
    try {
      await openPath(attachment.localPath);
    } catch (error) {
      sendError = error instanceof Error ? error.message : String(error);
    }
  }

  async function cancelTransfer(messageId: string) {
    if (previewMode) return;
    sendError = "";
    try {
      network = await invoke<NetworkSnapshot>("cancel_message", { messageId });
    } catch (error) {
      sendError = error instanceof Error ? error.message : String(error);
      await refreshNetwork().catch(() => undefined);
    }
  }

  async function retryMessage(messageId: string) {
    if (previewMode) return;
    sendError = "";
    try {
      await invoke<ChatMessage>("retry_message", { messageId });
      await refreshNetwork();
    } catch (error) {
      sendError = error instanceof Error ? error.message : String(error);
      await refreshNetwork().catch(() => undefined);
    }
  }

  function showMessageMenu(messageId: string, x: number, y: number) {
    messageMenu = { messageId, x, y };
  }

  function openMessageMenu(event: MouseEvent, messageId: string) {
    event.preventDefault();
    event.stopPropagation();
    showMessageMenu(messageId, event.clientX, event.clientY);
  }

  function clearMessageLongPress() {
    if (messageLongPressTimer !== null) window.clearTimeout(messageLongPressTimer);
    messageLongPressTimer = null;
    messageLongPressStart = null;
  }

  function beginMessageLongPress(event: PointerEvent, messageId: string) {
    if (event.pointerType !== "touch") return;
    clearMessageLongPress();
    messageLongPressTriggered = "";
    messageLongPressStart = {
      pointerId: event.pointerId,
      messageId,
      x: event.clientX,
      y: event.clientY,
    };
    messageLongPressTimer = window.setTimeout(() => {
      const press = messageLongPressStart;
      if (!press || press.pointerId !== event.pointerId) return;
      messageLongPressTriggered = press.messageId;
      showMessageMenu(press.messageId, press.x, press.y);
      messageLongPressTimer = null;
      messageLongPressStart = null;
    }, 520);
  }

  function moveMessageLongPress(event: PointerEvent) {
    const press = messageLongPressStart;
    if (!press || press.pointerId !== event.pointerId) return;
    if (Math.hypot(event.clientX - press.x, event.clientY - press.y) > 10) {
      clearMessageLongPress();
    }
  }

  function endMessageLongPress(event: PointerEvent, messageId: string) {
    clearMessageLongPress();
    if (messageLongPressTriggered !== messageId) return;
    event.preventDefault();
    window.setTimeout(() => {
      if (messageLongPressTriggered === messageId) messageLongPressTriggered = "";
    }, 0);
  }

  async function deleteSelectedMessage() {
    const selected = messageMenu;
    if (!selected || deletingMessage) return;
    deletingMessage = true;
    sendError = "";
    try {
      if (previewMode) {
        network = {
          ...network,
          messages: network.messages.filter((message) => message.messageId !== selected.messageId),
        };
      } else {
        network = await invoke<NetworkSnapshot>("delete_message", { messageId: selected.messageId });
      }
      messageMenu = null;
    } catch (error) {
      sendError = error instanceof Error ? error.message : String(error);
      await refreshNetwork().catch(() => undefined);
    } finally {
      deletingMessage = false;
    }
  }

  function handleWindowKeydown(event: KeyboardEvent) {
    if (event.key === "Escape") {
      activeImage = null;
      messageMenu = null;
      nicknameEditorOpen = false;
    }
  }

  onMount(() => {
    let active = true;
    let networkInterval = 0;
    let connectivityInterval = 0;
    let viewportFrame = 0;
    const visualViewport = window.visualViewport;

    function syncAppViewport() {
      window.cancelAnimationFrame(viewportFrame);
      viewportFrame = window.requestAnimationFrame(() => {
        const height = visualViewport?.height ?? window.innerHeight;
        const offsetTop = visualViewport?.offsetTop ?? 0;
        document.documentElement.style.setProperty("--tossit-viewport-height", `${Math.round(height)}px`);
        document.documentElement.style.setProperty("--tossit-viewport-top", `${Math.round(offsetTop)}px`);
        if (document.activeElement?.closest(".composer")) void scrollConversationToBottom();
      });
    }

    function syncKeyboardState() {
      const keyboardVisible = Boolean(document.activeElement?.matches(".composer input"));
      document.documentElement.classList.toggle("tossit-keyboard-visible", keyboardVisible);
      syncAppViewport();
      if (keyboardVisible) window.setTimeout(() => void scrollConversationToBottom(), 180);
    }

    function handleFocusOut() {
      window.setTimeout(syncKeyboardState, 0);
    }

    syncAppViewport();
    visualViewport?.addEventListener("resize", syncAppViewport);
    visualViewport?.addEventListener("scroll", syncAppViewport);
    window.addEventListener("resize", syncAppViewport);
    window.addEventListener("focusin", syncKeyboardState);
    window.addEventListener("focusout", handleFocusOut);

    async function load() {
      try {
        const nextIdentity = await invoke<DeviceIdentity>("device_identity");
        const nextConnectivity = await invoke<AppleConnectivity>("current_connectivity").catch(() => ({
          kind: "offline" as const,
          permission: "prompt" as const,
          ssid: null,
          networkId: null,
          canMessage: false,
        }));
        const nextNetwork = await invoke<NetworkSnapshot>("network_snapshot");
        if (!active) return;
        identity = nextIdentity;
        connectivity = nextConnectivity;
        network = nextNetwork;
        scheduleAvatarSync(nextNetwork);
        selectedNetworkId = defaultNetworkId(nextNetwork);
        networkInterval = window.setInterval(() => {
          refreshNetwork().catch(() => undefined);
        }, 1_000);
        connectivityInterval = window.setInterval(() => {
          refreshConnectivity().catch(() => undefined);
        }, 4_000);
      } catch {
        if (!active) return;
        previewMode = true;
        identity = {
          peerId: "preview-device",
          displayId: "62B9-A133-50BE",
          publicKey: "",
          nickname: "我的 Mac",
          avatarHash: "preview-avatar",
          avatarPath: previewAvatar,
        };
        connectivity = previewConnectivity;
        network = previewSnapshot;
        selectedNetworkId = defaultNetworkId(previewSnapshot);
        if (window.matchMedia("(min-width: 761px)").matches) {
          selectedPeerId = previewSnapshot.peers[0]?.peerId ?? "";
          void scrollConversationToBottom();
        }
      }
    }

    load();
    return () => {
      active = false;
      window.cancelAnimationFrame(viewportFrame);
      window.clearInterval(networkInterval);
      window.clearInterval(connectivityInterval);
      visualViewport?.removeEventListener("resize", syncAppViewport);
      visualViewport?.removeEventListener("scroll", syncAppViewport);
      window.removeEventListener("resize", syncAppViewport);
      window.removeEventListener("focusin", syncKeyboardState);
      window.removeEventListener("focusout", handleFocusOut);
      document.documentElement.classList.remove("tossit-keyboard-visible");
      document.documentElement.style.removeProperty("--tossit-viewport-height");
      document.documentElement.style.removeProperty("--tossit-viewport-top");
    };
  });
</script>

<svelte:window onkeydown={handleWindowKeydown} />

<svelte:head>
  <title>TossIt · 同一 Wi-Fi 对话</title>
</svelte:head>

<main class:peer-selected={Boolean(selectedPeerId)}>
  <aside class="device-panel" aria-label="设备与对话">
    <header class="sidebar-header">
      <label class:current={isSelectedNetworkCurrent()} class="network-switch">
        <select
          value={selectedNetworkId}
          onchange={handleNetworkSelect}
          disabled={selectableNetworks().length === 0}
          aria-label="切换 Wi-Fi 对话"
        >
          {#each selectableNetworks() as choice (choice.networkId)}
            <option value={choice.networkId}>{choice.displayName}</option>
          {:else}
            <option value="">{connectivityTitle()}</option>
          {/each}
        </select>
        <svg class="switch-chevron" viewBox="0 0 24 24" aria-hidden="true"><path d="m8 10 4 4 4-4" /></svg>
      </label>
      <span class:active={isSelectedNetworkCurrent()} class="scan-state-dot" aria-label={isSelectedNetworkCurrent() ? "正在发现设备" : "历史对话"}>
        <i></i>
      </span>
      <button class="profile-button" type="button" onclick={openNicknameEditor} aria-label="我的资料">
        <span class:has-image={Boolean(identity?.avatarPath)} class="identity-avatar" aria-hidden="true">
          {#if identity?.avatarPath}<img src={assetUrl(identity.avatarPath)} alt="" />{:else}<i></i>{/if}
        </span>
      </button>
    </header>

    {#if selectedNetwork()}
      {@const activeSpace = selectedNetwork()!}
      {@const conversations = conversationPeers()}
      {@const nearby = nearbyPeers()}
      <div class="device-content">
        {#if isSelectedNetworkCurrent() && conversations.length === 0 && nearby.length === 0}
          <section class="device-discovery-state">
            <span class="signal-field" aria-hidden="true"><i></i><b></b><em></em></span>
            <h2>正在查找设备</h2>
            <p>让另一台设备也连接“{activeSpace.displayName}”，然后打开 TossIt。</p>
            <div class="discovery-manual">
              <button class="manual-toggle" type="button" aria-expanded={manualConnectOpen} onclick={toggleManualConnect}>
                {manualConnectOpen ? "收起手动连接" : "没有找到？手动连接"}
              </button>
              {#if manualConnectOpen}
                <form class="manual-connect" onsubmit={connectManualEndpoint}>
                  <div class="manual-connect-copy">
                    <strong>输入对方地址</strong>
                    <span>自动发现找不到时使用</span>
                  </div>
                  <div class="manual-connect-row">
                    <input
                      bind:value={manualEndpoint}
                      aria-label="对方 IP 和端口"
                      placeholder="192.168.1.8:42318"
                      maxlength="128"
                      autocomplete="off"
                      spellcheck="false"
                    />
                    <button type="submit" disabled={!manualEndpoint.trim() || connectingEndpoint || previewMode}>
                      {connectingEndpoint ? "连接中" : "连接"}
                    </button>
                  </div>
                  <p>本机地址：{network.localEndpoints[0] ?? `端口 ${network.listeningPort || "—"}`}</p>
                  {#if manualConnectError}<p class="manual-connect-error" role="alert">{manualConnectError}</p>{/if}
                </form>
              {/if}
            </div>
          </section>
        {:else}
          {#if conversations.length > 0}
            <section class="peer-section">
              <div class="section-heading">
                <p class="section-label">对话</p>
                <span>{conversations.length}</span>
              </div>
              <div class="peer-list">
                {#each conversations as peer (peer.peerId)}
                  {@const recent = lastMessage(peer.peerId)}
                  {@const unread = peerUnread(peer.peerId)}
                  <button
                    class:chosen={selectedPeerId === peer.peerId}
                    class="peer-row"
                    type="button"
                    onclick={() => selectPeer(peer.peerId)}
                    aria-current={selectedPeerId === peer.peerId ? "true" : undefined}
                  >
                    <span class:has-image={Boolean(peer.avatarPath)} class:online={isSelectedNetworkCurrent() && peer.isOnline} class="peer-avatar" aria-hidden="true">
                      {#if peer.avatarPath}<img src={assetUrl(peer.avatarPath)} alt="" />{:else}<i></i><b></b>{/if}
                    </span>
                    <span class="peer-copy">
                      <span class="peer-title">
                        <strong>{peerName(peer)}</strong>
                        <span class="peer-title-meta">
                          {#if unread > 0}<b class="peer-unread" aria-label={`${unread} 条未读`}>{Math.min(unread, 99)}</b>{/if}
                          {#if recent}<time>{formatTime(recent.createdAtUnixMs)}</time>{/if}
                        </span>
                      </span>
                      <span class="peer-preview">{peer.displayId} · {messagePreview(recent)}</span>
                    </span>
                  </button>
                {/each}
              </div>
            </section>
          {/if}

          {#if isSelectedNetworkCurrent() && nearby.length > 0}
            <section class="peer-section nearby-section">
              <div class="section-heading">
                <p class="section-label">可连接的设备</p>
                <span>{nearby.length}</span>
              </div>
              <div class="peer-list">
                {#each nearby as peer (peer.peerId)}
                <button
                  class:chosen={selectedPeerId === peer.peerId}
                  class="peer-row"
                  type="button"
                  onclick={() => selectPeer(peer.peerId)}
                >
                  <span class:has-image={Boolean(peer.avatarPath)} class:online={peer.isOnline} class="peer-avatar" aria-hidden="true">
                    {#if peer.avatarPath}<img src={assetUrl(peer.avatarPath)} alt="" />{:else}<i></i><b></b>{/if}
                  </span>
                  <span class="peer-copy">
                    <span class="peer-title"><strong>{peerName(peer)}</strong></span>
                    <span class="peer-preview">
                      {peer.displayId} · {peer.trustState === "trusted" ? "可以开始传输" : peer.trustState === "blocked" ? "已拒绝" : "待确认"}
                    </span>
                  </span>
                </button>
                {/each}
              </div>
            </section>
          {/if}

          {#if isSelectedNetworkCurrent()}
            <section class="manual-section">
              <button class="manual-toggle" type="button" aria-expanded={manualConnectOpen} onclick={toggleManualConnect}>
                {manualConnectOpen ? "收起手动连接" : "没有找到？手动连接"}
              </button>
              {#if manualConnectOpen}
                <form class="manual-connect" onsubmit={connectManualEndpoint}>
                  <div class="manual-connect-copy">
                    <strong>输入对方地址</strong>
                    <span>自动发现找不到时使用</span>
                  </div>
                  <div class="manual-connect-row">
                    <input
                      bind:value={manualEndpoint}
                      aria-label="对方 IP 和端口"
                      placeholder="192.168.1.8:42318"
                      maxlength="128"
                      autocomplete="off"
                      spellcheck="false"
                    />
                    <button type="submit" disabled={!manualEndpoint.trim() || connectingEndpoint || previewMode}>
                      {connectingEndpoint ? "连接中" : "连接"}
                    </button>
                  </div>
                  <p>本机地址：{network.localEndpoints[0] ?? `端口 ${network.listeningPort || "—"}`}</p>
                  {#if manualConnectError}<p class="manual-connect-error" role="alert">{manualConnectError}</p>{/if}
                </form>
              {/if}
            </section>
          {/if}
        {/if}
      </div>
    {:else}
      <div class="device-placeholder">
        <span class="signal-field compact" aria-hidden="true"><i></i><b></b><em></em></span>
        <h2>{connectivityTitle()}</h2>
        <p>{connectivityDescription()}</p>
        {#if connectivity.permission === "prompt"}
          <button type="button" disabled={requestingNetworkAccess || previewMode} onclick={requestNetworkAccess}>
            {requestingNetworkAccess ? "等待允许" : "允许识别 Wi-Fi"}
          </button>
        {/if}
        {#if networkAccessError}<p class="network-access-error" role="status">{networkAccessError}</p>{/if}
      </div>
    {/if}
  </aside>

  <section class="conversation-panel" aria-label="对话">
    {#if currentPeer()}
      {@const peer = currentPeer()!}
      <header class="conversation-header">
        <button class="back-button" type="button" onclick={() => (selectedPeerId = "")} aria-label="返回设备列表">
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="m15 18-6-6 6-6" /></svg>
        </button>
        <span class:has-image={Boolean(peer.avatarPath)} class:online={isSelectedNetworkCurrent() && peer.isOnline} class="mini-avatar" aria-hidden="true">
          {#if peer.avatarPath}<img src={assetUrl(peer.avatarPath)} alt="" />{:else}<i></i>{/if}
        </span>
        <div class="conversation-title">
          <strong>{peerName(peer)}</strong>
          <span>{peer.displayId} · {isSelectedNetworkCurrent() ? (peer.isOnline ? "当前 Wi-Fi 在线" : "暂时离线 · 可留言") : `${selectedNetwork()?.displayName ?? "这个 Wi-Fi"} · 仅查看`}</span>
        </div>
        {#if isSelectedNetworkCurrent()}
          <span class="secure-state">
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7 10V7a5 5 0 0 1 10 0v3M6 10h12v10H6z" /></svg>
            {peer.trustState === "trusted" ? "已信任 · 加密直连" : "等待设备确认"}
          </span>
        {/if}
      </header>

      <div
        bind:this={messageScrollElement}
        class:pairing={isSelectedNetworkCurrent() && peer.trustState !== "trusted"}
        class="message-scroll"
        aria-live="polite"
      >
        {#if isSelectedNetworkCurrent() && peer.trustState !== "trusted"}
          <section class:blocked={peer.trustState === "blocked"} class="trust-card" aria-label="设备确认">
            <p class="eyebrow">设备校验码</p>
            <strong>{peer.verificationCode}</strong>
            <h2>{peer.trustState === "blocked" ? "这台设备已被拒绝" : "确认是你认识的设备"}</h2>
            <p>在另一台设备上打开 TossIt。两边显示的校验码一致时，双方各自点一次确认。</p>
            <div class="trust-actions">
              <button type="button" disabled={updatingTrust} onclick={() => updatePeerTrust(peer.peerId, true)}>
                {peer.trustState === "blocked" ? "重新信任" : "校验码一致，确认设备"}
              </button>
              {#if peer.trustState === "discovered"}
                <button class="secondary" type="button" disabled={updatingTrust} onclick={() => updatePeerTrust(peer.peerId, false)}>拒绝</button>
              {/if}
            </div>
          </section>
        {:else if messagesFor(peer.peerId).length === 0}
          <div class="conversation-empty">
            <span class="signal-field compact" aria-hidden="true"><i></i><b></b><em></em></span>
            <h2>开始局域网对话</h2>
            <p>{isSelectedNetworkCurrent() ? "消息直接发给这台设备，不经过云端。" : "这个网络下还没有与这台设备的消息。"}</p>
          </div>
        {:else}
          {#if !previewMode && historyHasMore[conversationKey(peer.peerId)] !== false}
            <button class="history-button" type="button" disabled={historyLoading} onclick={loadOlderMessages}>
              {historyLoading ? "加载中" : "更早消息"}
            </button>
          {/if}
          <div class="message-list">
            {#each messagesFor(peer.peerId) as message (message.messageId)}
              <article
                class:outgoing={message.direction === "outgoing"}
                class="message-row"
                oncontextmenu={(event) => openMessageMenu(event, message.messageId)}
                onpointerdown={(event) => beginMessageLongPress(event, message.messageId)}
                onpointermove={moveMessageLongPress}
                onpointerup={(event) => endMessageLongPress(event, message.messageId)}
                onpointercancel={clearMessageLongPress}
              >
                <div class:attachment-bubble={message.content.type === "attachment"} class="bubble">
                  {#if message.content.type === "text"}
                    <p>{message.content.text}</p>
                  {:else}
                    {@const attachment = message.content.attachment}
                    {#if attachment.kind === "image"}
                      <button
                        class="image-card"
                        type="button"
                        disabled={!attachment.localPath}
                        onclick={() => {
                          if (messageLongPressTriggered !== message.messageId && attachment.localPath) {
                            activeImage = attachment;
                          }
                        }}
                        aria-label={`预览图片 ${attachment.fileName}`}
                      >
                        {#if attachment.previewPath || attachment.localPath}
                          <img
                            src={assetUrl(attachment.previewPath ?? attachment.localPath)}
                            alt={attachment.fileName}
                          />
                        {:else}
                          <span class="image-placeholder" aria-hidden="true"></span>
                        {/if}
                        <span class="attachment-label">{attachment.fileName}</span>
                      </button>
                    {:else}
                      <button
                        class="file-card"
                        type="button"
                        disabled={!attachment.localPath}
                        onclick={() => openFile(attachment)}
                        aria-label={`打开文件 ${attachment.fileName}`}
                      >
                        <span class="file-icon" aria-hidden="true">
                          <svg viewBox="0 0 24 24"><path d="M6 3h8l4 4v14H6zM14 3v5h5" /></svg>
                        </span>
                        <span class="file-copy">
                          <strong>{attachment.fileName}</strong>
                          <span>{formatBytes(attachment.byteSize)}</span>
                        </span>
                        <svg class="file-open" viewBox="0 0 24 24" aria-hidden="true"><path d="m9 18 6-6-6-6" /></svg>
                      </button>
                    {/if}
                    {#if message.delivery === "sending" || message.delivery === "receiving"}
                      <span class="transfer-progress">
                        <i style={`width: ${transferPercent(attachment)}%`}></i>
                      </span>
                    {/if}
                  {/if}
                  <span class="message-meta">
                    {formatTime(message.createdAtUnixMs)}
                    {#if message.content.type === "attachment" || message.direction === "outgoing"}
                      · {deliveryText(message)}
                    {/if}
                    {#if message.direction === "outgoing" && message.content.type === "attachment" && message.delivery === "sending"}
                      · <button class="message-action" type="button" onclick={() => cancelTransfer(message.messageId)}>取消</button>
                    {:else if message.direction === "outgoing" && message.delivery === "failed"}
                      · <button class="message-action" type="button" onclick={() => retryMessage(message.messageId)}>重试</button>
                    {/if}
                  </span>
                </div>
              </article>
            {/each}
          </div>
        {/if}
      </div>

      <form class="composer" onsubmit={submitMessage}>
        {#if sendError}<p class="send-error" role="alert">{sendError}</p>{/if}
        <div class="composer-row">
          <div class="attachment-tools" aria-label="添加附件">
            <button
              class="tool-button"
              type="button"
              disabled={!canSendTo(peer) || picking || sending}
              onclick={() => pickAttachment("image")}
              aria-label="发送图片"
              title="发送图片"
            >
              <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 5h16v14H4zM8 10a2 2 0 1 0 0-4 2 2 0 0 0 0 4Zm-4 7 5-5 3 3 2-2 6 6" /></svg>
            </button>
            <button
              class="tool-button"
              type="button"
              disabled={!canSendTo(peer) || picking || sending}
              onclick={() => pickAttachment("file")}
              aria-label="发送文件"
              title="发送文件"
            >
              <svg viewBox="0 0 24 24" aria-hidden="true"><path d="m9 12 5-5a3 3 0 0 1 4 4l-7 7a5 5 0 0 1-7-7l7-7" /></svg>
            </button>
          </div>
          <input
            bind:value={draft}
            aria-label="消息内容"
            placeholder={composerPlaceholder(peer)}
            maxlength="65536"
            disabled={!canSendTo(peer) || picking}
            autocomplete="off"
          />
          <button type="submit" disabled={!canSendTo(peer) || !draft.trim() || sending} aria-label="发送">
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="m5 12 14-7-4 14-3-5-7-2Zm7 2 7-9" /></svg>
          </button>
        </div>
        <small>{composerHint(peer)}</small>
      </form>
    {:else}
      <div class="no-selection">
        <p>选择一个设备</p>
      </div>
    {/if}
  </section>
</main>

{#if nicknameEditorOpen && identity}
  <div
    class="profile-overlay"
    role="presentation"
    onclick={(event) => event.currentTarget === event.target && (nicknameEditorOpen = false)}
  >
    <form class="profile-editor" aria-label="修改我的资料" onsubmit={saveNickname}>
      <p class="eyebrow">附近的人会看到</p>
      <h2>我的资料</h2>
      <div class="avatar-editor">
        <button class:has-image={Boolean(identity.avatarPath)} class="profile-avatar" type="button" disabled={avatarSaving} onclick={chooseAvatar} aria-label={identity.avatarPath ? "更换头像" : "设置头像"}>
          {#if identity.avatarPath}<img src={assetUrl(identity.avatarPath)} alt="我的头像" />{:else}<i></i>{/if}
        </button>
        <div>
          <button class="avatar-action" type="button" disabled={avatarSaving} onclick={chooseAvatar}>
            {avatarSaving ? "处理中" : identity.avatarPath ? "更换头像" : "设置头像"}
          </button>
          {#if identity.avatarPath}
            <button class="avatar-remove" type="button" disabled={avatarSaving} onclick={removeAvatar}>移除</button>
          {/if}
          <span>自动裁成正方形，仅在可信设备间传输</span>
        </div>
      </div>
      <label class="nickname-field">
        <span>昵称</span>
      <input
        bind:value={nicknameDraft}
        aria-label="我的昵称"
        maxlength="24"
        autocomplete="off"
      />
      </label>
      <p class="profile-id">设备短码 {identity.displayId}</p>
      {#if storageSummary?.receivedFileCount}
        <div class="storage-row">
          <span>已接收文件 {formatBytes(storageSummary.receivedBytes)}</span>
          <button type="button" disabled={clearingStorage} onclick={clearReceivedFiles}>
            {clearingStorage ? "清理中" : "清理"}
          </button>
        </div>
      {/if}
      {#if nicknameError}<p class="profile-error" role="alert">{nicknameError}</p>{/if}
      <div class="profile-actions">
        <button class="secondary" type="button" disabled={avatarSaving} onclick={() => (nicknameEditorOpen = false)}>取消</button>
        <button type="submit" disabled={!nicknameDraft.trim() || nicknameSaving || avatarSaving}>
          {nicknameSaving ? "保存中" : "保存"}
        </button>
      </div>
    </form>
  </div>
{/if}

{#if messageMenu}
  <div
    class="message-menu-overlay"
    role="presentation"
    onclick={(event) => event.currentTarget === event.target && !deletingMessage && (messageMenu = null)}
  >
    <div
      class="message-menu"
      role="menu"
      style={`--message-menu-x: ${messageMenu.x}px; --message-menu-y: ${messageMenu.y}px;`}
    >
      <button type="button" role="menuitem" disabled={deletingMessage} onclick={deleteSelectedMessage}>
        {deletingMessage ? "删除中" : "删除"}
      </button>
    </div>
  </div>
{/if}

{#if activeImage}
  <div
    class="image-viewer"
    role="dialog"
    aria-modal="true"
    aria-label={`图片预览 ${activeImage.fileName}`}
    tabindex="-1"
    onclick={(event) => event.currentTarget === event.target && (activeImage = null)}
    onkeydown={(event) => event.key === "Escape" && (activeImage = null)}
  >
    <button class="image-viewer-close" type="button" onclick={() => (activeImage = null)} aria-label="关闭图片预览">
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="m6 6 12 12M18 6 6 18" /></svg>
    </button>
    <img src={assetUrl(activeImage.localPath)} alt={activeImage.fileName} />
  </div>
{/if}

<style>
  :global(*) {
    box-sizing: border-box;
  }

  :global(html) {
    color-scheme: light;
    font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI", sans-serif;
    background: #e9edf5;
    color: #172033;
    font-synthesis: none;
    text-rendering: optimizeLegibility;
  }

  :global(body) {
    min-width: 320px;
    min-height: 100vh;
    margin: 0;
    overflow: hidden;
    background: #e9edf5;
  }

  :global(button),
  :global(input) {
    font: inherit;
  }

  :global(button) {
    -webkit-tap-highlight-color: transparent;
  }

  :global(button:focus-visible),
  :global(input:focus-visible) {
    outline: 3px solid rgba(39, 93, 255, 0.28);
    outline-offset: 2px;
  }

  main {
    width: min(1240px, calc(100% - 20px));
    height: min(860px, calc(100dvh - 20px));
    min-height: 0;
    margin: 10px auto;
    display: grid;
    grid-template-columns: clamp(310px, 31vw, 380px) minmax(0, 1fr);
    overflow: hidden;
    border: 1px solid #d9dee8;
    border-radius: 16px;
    background: #f7f8fa;
    box-shadow: 0 18px 52px rgba(31, 42, 64, 0.14);
  }

  .device-panel {
    min-width: 0;
    min-height: 0;
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    grid-template-rows: auto minmax(0, 1fr);
    border-right: 1px solid #dfe4ee;
    background: #f8f9fb;
  }

  .conversation-header {
    height: 64px;
    display: flex;
    align-items: center;
  }

  .sidebar-header {
    min-width: 0;
    height: 64px;
    padding: 0 14px;
    display: flex;
    align-items: center;
    gap: 8px;
    border-bottom: 1px solid #e1e6ef;
    background: rgba(248, 249, 251, 0.96);
  }

  .device-content {
    min-width: 0;
    min-height: 0;
    overflow-y: auto;
  }

  .network-switch {
    position: relative;
    min-width: 0;
    height: 38px;
    max-width: calc(100% - 50px);
    padding: 0 30px 0 12px;
    display: flex;
    align-items: center;
    border: 1px solid #dce2ec;
    border-radius: 10px;
    background: #fff;
  }

  .network-switch.current {
    border-color: #ccd8fb;
    background: #eef3ff;
  }

  .network-switch select {
    min-width: 0;
    max-width: 100%;
    height: 100%;
    padding: 0;
    overflow: hidden;
    border: 0;
    outline: 0;
    appearance: none;
    background: transparent;
    color: #27324a;
    font-size: 0.84rem;
    font-weight: 700;
    text-overflow: ellipsis;
  }

  .network-switch select:disabled {
    color: #707b90;
    opacity: 1;
  }

  .network-switch .switch-chevron {
    position: absolute;
    right: 8px;
    width: 17px;
    fill: none;
    stroke: #7c879c;
    stroke-linecap: round;
    stroke-linejoin: round;
    stroke-width: 1.8;
    pointer-events: none;
  }

  .scan-state-dot {
    width: 12px;
    height: 38px;
    flex: none;
    display: grid;
    place-items: center;
  }

  .scan-state-dot i {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: #aab2c2;
  }

  .scan-state-dot.active i {
    background: #2daa76;
    box-shadow: 0 0 0 4px rgba(45, 170, 118, 0.11);
    animation: pulse 1.8s ease-in-out infinite;
  }

  .profile-button {
    width: 38px;
    height: 38px;
    margin-left: auto;
    padding: 3px;
    flex: none;
    display: grid;
    place-items: center;
    border: 0;
    border-radius: 12px;
    background: transparent;
    cursor: pointer;
  }

  .profile-button .identity-avatar {
    width: 32px;
    height: 32px;
    border-radius: 10px;
  }

  .section-label {
    margin: 0;
    color: #7c8699;
    font-size: 0.64rem;
    font-weight: 760;
    letter-spacing: 0.08em;
  }

  .section-heading {
    min-height: 24px;
    padding: 0 9px 6px;
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .section-heading > span {
    color: #9aa3b2;
    font-size: 0.62rem;
    font-variant-numeric: tabular-nums;
  }

  .peer-row.chosen::after {
    content: "";
    position: absolute;
    top: 12px;
    bottom: 12px;
    left: 0;
    width: 3px;
    border-radius: 0 3px 3px 0;
    background: #3370ff;
  }

  .network-access-error {
    margin: 8px 9px 0;
    color: #b84150;
    font-size: 0.64rem;
    line-height: 1.45;
  }

  .back-button {
    width: 34px;
    height: 34px;
    margin-left: -7px;
    flex: none;
    display: none;
    place-items: center;
    border: 0;
    background: transparent;
    color: #35415a;
    cursor: pointer;
  }

  .back-button svg {
    width: 22px;
    fill: none;
    stroke: currentColor;
    stroke-linecap: round;
    stroke-linejoin: round;
    stroke-width: 1.8;
  }

  .device-content {
    padding: 14px 0 18px;
  }

  .device-content > .peer-section,
  .device-content > .manual-section {
    width: min(620px, calc(100% - 28px));
    margin-right: auto;
    margin-left: auto;
  }

  .device-discovery-state {
    width: min(480px, calc(100% - 32px));
    min-height: 100%;
    margin: 0 auto;
    padding: 34px 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    text-align: center;
  }

  .device-discovery-state h2 {
    margin: 22px 0 8px;
    color: #273047;
    font-size: 1.05rem;
    letter-spacing: -0.025em;
  }

  .device-discovery-state > p {
    max-width: 360px;
    margin: 0;
    color: #727d94;
    font-size: 0.78rem;
    line-height: 1.65;
  }

  .discovery-manual {
    width: 100%;
    margin-top: 28px;
  }

  .discovery-manual > .manual-toggle {
    border-color: transparent;
    background: transparent;
  }

  .discovery-manual .manual-connect {
    margin-top: 10px;
    text-align: left;
  }

  .peer-section + .peer-section,
  .manual-section {
    margin-top: 16px;
  }

  .device-content .peer-list {
    padding: 0 10px;
    overflow: visible;
  }

  .nearby-section {
    padding-top: 14px;
    border-top: 1px solid #e1e6ef;
  }

  .manual-section {
    padding: 14px 10px 0;
    border-top: 1px solid #e1e6ef;
  }

  .manual-section > .manual-toggle {
    width: 100%;
    min-height: 36px;
  }

  .manual-section .manual-connect {
    margin-top: 9px;
  }

  .device-placeholder {
    grid-row: 2;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    color: #7c8699;
    font-size: 0.74rem;
  }

  .device-placeholder h2 {
    margin: 18px 0 7px;
    color: #273047;
    font-size: 1rem;
  }

  .device-placeholder p {
    max-width: 280px;
    margin: 0;
    color: #727d94;
    line-height: 1.6;
    text-align: center;
  }

  .device-placeholder > button {
    min-height: 38px;
    margin-top: 18px;
    padding: 0 16px;
    border: 0;
    border-radius: 10px;
    background: #275dff;
    color: #fff;
    font-size: 0.74rem;
    font-weight: 700;
  }

  .device-placeholder > button:disabled {
    background: #aeb8cb;
  }

  .identity-avatar {
    position: relative;
    width: 24px;
    height: 24px;
    flex: none;
    display: grid;
    place-items: center;
    border-radius: 7px;
    background: #e3e9f7;
    color: #4774ef;
  }

  .identity-avatar::before,
  .identity-avatar i {
    content: "";
    position: absolute;
    border: 1.2px solid currentColor;
    border-radius: 50%;
  }

  .identity-avatar::before {
    width: 14px;
    height: 14px;
  }

  .identity-avatar i {
    width: 4px;
    height: 4px;
    background: currentColor;
  }

  .identity-avatar img,
  .peer-avatar img,
  .mini-avatar img,
  .profile-avatar img {
    width: 100%;
    height: 100%;
    display: block;
    border-radius: inherit;
    object-fit: cover;
  }

  .identity-avatar.has-image::before {
    display: none;
  }

  .manual-toggle {
    padding: 5px 8px;
    border: 1px solid #d5dce8;
    border-radius: 7px;
    background: #fff;
    color: #536078;
    font-size: 0.67rem;
    font-weight: 680;
    cursor: pointer;
  }

  .manual-toggle:hover {
    border-color: #b9c9ff;
    color: #275dff;
  }

  h2,
  p {
    margin-top: 0;
  }

  .manual-connect {
    padding: 12px;
    border: 1px solid #dce2ec;
    border-radius: 12px;
    background: #fff;
    box-shadow: 0 7px 20px rgba(29, 43, 70, 0.05);
  }

  .manual-connect-copy {
    margin-bottom: 9px;
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 10px;
  }

  .manual-connect-copy strong {
    color: #27324a;
    font-size: 0.74rem;
  }

  .manual-connect-copy span,
  .manual-connect p {
    color: #8791a5;
    font-size: 0.62rem;
  }

  .manual-connect-row {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: 7px;
  }

  .manual-connect-row input {
    min-width: 0;
    padding: 8px 9px;
    border: 1px solid #d7deea;
    border-radius: 8px;
    background: #f9fafc;
    color: #27324a;
    font-size: 0.72rem;
    font-variant-numeric: tabular-nums;
  }

  .manual-connect-row input::placeholder {
    color: #a0a9ba;
  }

  .manual-connect-row button {
    min-width: 54px;
    padding: 0 10px;
    border: 0;
    border-radius: 8px;
    background: #275dff;
    color: #fff;
    font-size: 0.7rem;
    font-weight: 720;
    cursor: pointer;
  }

  .manual-connect-row button:disabled {
    background: #adb7cb;
    cursor: default;
  }

  .manual-connect p {
    margin: 8px 0 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .manual-connect p.manual-connect-error {
    color: #b84150;
    white-space: normal;
  }

  .peer-list {
    min-height: 0;
    padding: 5px 10px 14px;
    overflow-y: auto;
  }

  .peer-row {
    position: relative;
    width: 100%;
    padding: 11px 10px;
    display: flex;
    align-items: center;
    gap: 12px;
    border: 0;
    border-radius: 9px;
    background: transparent;
    color: inherit;
    text-align: left;
    cursor: pointer;
  }

  .peer-row:hover {
    background: #e9edf6;
  }

  .peer-row.chosen {
    background: #e8edff;
  }

  .peer-avatar,
  .mini-avatar {
    position: relative;
    flex: none;
    display: grid;
    place-items: center;
    border-radius: 50%;
    background: #e4e8f0;
    color: #8791a6;
  }

  .peer-avatar {
    width: 44px;
    height: 44px;
  }

  .peer-avatar::before,
  .peer-avatar i,
  .peer-avatar b,
  .mini-avatar::before,
  .mini-avatar i {
    content: "";
    position: absolute;
    border: 1.5px solid currentColor;
    border-radius: 50%;
  }

  .peer-avatar::before {
    width: 28px;
    height: 28px;
  }

  .peer-avatar i {
    width: 17px;
    height: 17px;
  }

  .peer-avatar b,
  .mini-avatar i {
    width: 5px;
    height: 5px;
    border: 0;
    background: currentColor;
  }

  .peer-avatar.online,
  .mini-avatar.online {
    background: #dfe8ff;
    color: #275dff;
  }

  .peer-avatar.online::after {
    content: "";
    position: absolute;
    right: 0;
    bottom: 2px;
    width: 9px;
    height: 9px;
    border: 2px solid #f3f5f9;
    border-radius: 50%;
    background: #2daa76;
  }

  .peer-avatar.has-image::before,
  .mini-avatar.has-image::before {
    display: none;
  }

  .peer-copy {
    min-width: 0;
    flex: 1;
  }

  .peer-title {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 10px;
  }

  .peer-title strong {
    overflow: hidden;
    font-size: 0.86rem;
    font-variant-numeric: tabular-nums;
    text-overflow: ellipsis;
  }

  .peer-title time {
    flex: none;
    color: #929aad;
    font-size: 0.64rem;
  }

  .peer-title-meta {
    flex: none;
    display: flex;
    align-items: center;
    gap: 7px;
  }

  .peer-unread {
    min-width: 18px;
    height: 18px;
    padding: 0 5px;
    display: grid;
    place-items: center;
    border-radius: 999px;
    background: #275dff;
    color: #fff;
    font-size: 0.62rem;
    font-weight: 700;
    line-height: 1;
  }

  .peer-preview {
    display: block;
    overflow: hidden;
    margin-top: 4px;
    color: #727d94;
    font-size: 0.74rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .conversation-empty h2 {
    margin: 22px 0 8px;
    font-size: 1rem;
    letter-spacing: -0.02em;
  }

  .conversation-empty p {
    max-width: 270px;
    margin-bottom: 0;
    color: #727d94;
    font-size: 0.78rem;
    line-height: 1.6;
  }

  .signal-field {
    position: relative;
    width: 76px;
    height: 76px;
    display: grid;
    place-items: center;
    color: #275dff;
  }

  .signal-field::before,
  .signal-field i,
  .signal-field b,
  .signal-field em {
    content: "";
    position: absolute;
    border: 1.5px solid currentColor;
    border-radius: 50%;
    opacity: 0.24;
  }

  .signal-field::before {
    width: 72px;
    height: 72px;
  }

  .signal-field i {
    width: 50px;
    height: 50px;
    opacity: 0.38;
  }

  .signal-field b {
    width: 28px;
    height: 28px;
    opacity: 0.65;
  }

  .signal-field em {
    width: 8px;
    height: 8px;
    border: 0;
    background: currentColor;
    opacity: 1;
  }

  .signal-field.compact {
    transform: scale(0.82);
  }

  .conversation-panel {
    min-width: 0;
    min-height: 0;
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    grid-template-rows: auto minmax(0, 1fr) auto;
    background: #fff;
  }

  .conversation-header {
    min-width: 0;
    padding: 0 18px;
    gap: 11px;
    border-bottom: 1px solid #e3e7ef;
  }

  .mini-avatar {
    width: 36px;
    height: 36px;
  }

  .mini-avatar::before {
    width: 21px;
    height: 21px;
  }

  .conversation-title {
    min-width: 0;
    flex: 1;
    overflow: hidden;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .conversation-title strong,
  .conversation-title span {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .conversation-title strong {
    font-size: 0.86rem;
    font-variant-numeric: tabular-nums;
  }

  .conversation-title span {
    color: #778197;
    font-size: 0.67rem;
  }

  .secure-state {
    flex: none;
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: 5px;
    color: #65708a;
    font-size: 0.68rem;
    white-space: nowrap;
  }

  .secure-state svg {
    width: 14px;
    fill: none;
    stroke: currentColor;
    stroke-linecap: round;
    stroke-linejoin: round;
    stroke-width: 1.7;
  }

  .back-button {
    display: none;
  }

  .message-scroll {
    min-width: 0;
    min-height: 0;
    overflow-x: hidden;
    overflow-y: auto;
    background: #f5f6f8;
  }

  .message-scroll.pairing {
    padding: 24px;
    display: grid;
    place-items: center;
  }

  .message-scroll.pairing .trust-card {
    margin: 0;
  }

  .trust-card {
    width: min(520px, calc(100% - 40px));
    margin: 28px auto 4px;
    padding: 24px;
    border: 1px solid #cbd8ff;
    border-radius: 18px;
    background: #fff;
    box-shadow: 0 12px 32px rgba(39, 93, 255, 0.08);
    text-align: center;
  }

  .trust-card.blocked {
    border-color: #e3d6d6;
    box-shadow: none;
  }

  .trust-card > strong {
    display: block;
    margin: 10px 0 16px;
    color: #275dff;
    font-size: clamp(1.7rem, 5vw, 2.35rem);
    font-variant-numeric: tabular-nums;
    letter-spacing: 0.12em;
  }

  .trust-card h2 {
    margin: 0;
    color: #273047;
    font-size: 1rem;
  }

  .trust-card > p:not(.eyebrow) {
    max-width: 390px;
    margin: 9px auto 0;
    color: #727d94;
    font-size: 0.76rem;
    line-height: 1.65;
  }

  .trust-actions {
    margin-top: 18px;
    display: flex;
    justify-content: center;
    gap: 9px;
  }

  .trust-actions button {
    min-height: 38px;
    padding: 0 16px;
    border: 1px solid #275dff;
    border-radius: 10px;
    background: #275dff;
    color: #fff;
    font: inherit;
    font-size: 0.74rem;
    font-weight: 650;
    cursor: pointer;
  }

  .trust-actions button.secondary {
    border-color: #d9deea;
    background: #fff;
    color: #65708a;
  }

  .trust-actions button:disabled {
    cursor: wait;
    opacity: 0.55;
  }

  .conversation-empty,
  .no-selection {
    height: 100%;
    padding: 32px;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    text-align: center;
  }

  .no-selection p {
    margin: 0;
    color: #8a93a5;
    font-size: 0.78rem;
  }

  .history-button {
    margin: 16px auto 0;
    padding: 5px 10px;
    border: 0;
    background: transparent;
    color: #7b8497;
    font-size: 0.68rem;
    cursor: pointer;
  }

  .history-button:disabled {
    cursor: default;
    opacity: 0.55;
  }

  .message-list {
    min-width: 0;
    width: min(690px, calc(100% - 40px));
    margin: 0 auto;
    padding: 22px 0 30px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .message-row {
    min-width: 0;
    display: flex;
    justify-content: flex-start;
  }

  .message-row.outgoing {
    justify-content: flex-end;
  }

  .bubble {
    min-width: 0;
    max-width: min(76%, 520px);
    padding: 10px 13px 8px;
    border: 1px solid #dfe4ed;
    border-radius: 4px 14px 14px 14px;
    background: #fff;
    box-shadow: 0 4px 14px rgba(31, 44, 70, 0.05);
  }

  .outgoing .bubble {
    border-color: #275dff;
    border-radius: 14px 4px 14px 14px;
    background: #275dff;
    color: #fff;
    box-shadow: 0 7px 20px rgba(39, 93, 255, 0.16);
  }

  .bubble.attachment-bubble,
  .outgoing .bubble.attachment-bubble {
    width: min(340px, 76vw);
    max-width: min(76%, 520px);
    padding: 5px;
    border-color: #d9e0ec;
    background: #fff;
    color: #172033;
    box-shadow: 0 7px 22px rgba(31, 44, 70, 0.08);
  }

  .outgoing .bubble.attachment-bubble {
    border-color: #b9c9ff;
  }

  .bubble p {
    margin-bottom: 5px;
    font-size: 0.9rem;
    line-height: 1.52;
    overflow-wrap: anywhere;
    white-space: pre-wrap;
  }

  .message-meta {
    display: block;
    color: #8b94a6;
    font-size: 0.62rem;
    text-align: right;
  }

  .outgoing .message-meta {
    color: rgba(255, 255, 255, 0.68);
  }

  .attachment-bubble .message-meta,
  .outgoing .attachment-bubble .message-meta {
    padding: 6px 7px 2px;
    color: #7c879d;
  }

  .message-action {
    margin: 0;
    padding: 0;
    border: 0;
    background: transparent;
    color: inherit;
    font: inherit;
    text-decoration: underline;
    text-underline-offset: 2px;
    cursor: pointer;
  }

  .image-card,
  .file-card {
    width: 100%;
    border: 0;
    color: inherit;
    cursor: pointer;
  }

  .image-card {
    position: relative;
    min-height: 168px;
    padding: 0;
    overflow: hidden;
    display: block;
    border-radius: 10px;
    background: #edf1f8;
  }

  .image-card img {
    width: 100%;
    max-height: 270px;
    display: block;
    object-fit: cover;
  }

  .image-card:disabled,
  .file-card:disabled {
    cursor: default;
  }

  .image-placeholder {
    width: 100%;
    height: 180px;
    display: block;
    background: #e7ebf3;
  }

  .attachment-label {
    position: absolute;
    right: 8px;
    bottom: 8px;
    max-width: calc(100% - 16px);
    overflow: hidden;
    padding: 5px 8px;
    border-radius: 6px;
    background: rgba(23, 32, 51, 0.78);
    color: #fff;
    font-size: 0.68rem;
    text-overflow: ellipsis;
    white-space: nowrap;
    backdrop-filter: blur(8px);
  }

  .file-card {
    min-height: 76px;
    padding: 11px 10px;
    display: flex;
    align-items: center;
    gap: 11px;
    border-radius: 10px;
    background: #f3f6fb;
    text-align: left;
  }

  .file-card:hover:not(:disabled) {
    background: #edf2ff;
  }

  .file-icon {
    width: 42px;
    height: 50px;
    flex: none;
    display: grid;
    place-items: center;
    border: 1px solid #c9d5fb;
    border-radius: 7px;
    background: #fff;
    color: #275dff;
  }

  .file-icon svg,
  .file-open {
    fill: none;
    stroke: currentColor;
    stroke-linecap: round;
    stroke-linejoin: round;
    stroke-width: 1.7;
  }

  .file-icon svg {
    width: 22px;
  }

  .file-copy {
    min-width: 0;
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 5px;
  }

  .file-copy strong {
    overflow: hidden;
    font-size: 0.79rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .file-copy span {
    color: #7a859b;
    font-size: 0.66rem;
    font-variant-numeric: tabular-nums;
  }

  .file-open {
    width: 16px;
    flex: none;
    color: #8792a8;
  }

  .transfer-progress {
    height: 3px;
    margin: 5px 3px 0;
    overflow: hidden;
    display: block;
    border-radius: 99px;
    background: #dfe5f0;
  }

  .transfer-progress i {
    height: 100%;
    display: block;
    border-radius: inherit;
    background: #275dff;
    transition: width 160ms ease-out;
  }

  .composer {
    min-width: 0;
    padding: 14px 20px 16px;
    border-top: 1px solid #e3e7ef;
    background: #fff;
  }

  .composer-row {
    min-width: 0;
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .composer input {
    min-width: 0;
    height: 44px;
    flex: 1;
    padding: 0 14px;
    border: 1px solid #d8dfeb;
    border-radius: 12px;
    background: #f7f8fb;
    color: #172033;
  }

  .composer input::placeholder {
    color: #9aa3b5;
  }

  .composer input:disabled {
    opacity: 0.72;
  }

  .composer-row > button {
    width: 44px;
    height: 44px;
    flex: none;
    display: grid;
    place-items: center;
    border: 0;
    border-radius: 12px;
    background: #275dff;
    color: #fff;
    cursor: pointer;
  }

  .composer-row > button:disabled {
    background: #c8cfdd;
    cursor: default;
  }

  .composer button svg {
    width: 20px;
    fill: none;
    stroke: currentColor;
    stroke-linecap: round;
    stroke-linejoin: round;
    stroke-width: 1.8;
  }

  .attachment-tools {
    display: flex;
    gap: 5px;
  }

  .attachment-tools .tool-button {
    width: 36px;
    height: 36px;
    padding: 0;
    display: grid;
    place-items: center;
    border: 1px solid #d8dfeb;
    border-radius: 10px;
    background: #f7f8fb;
    color: #65708a;
    cursor: pointer;
  }

  .attachment-tools .tool-button:hover:not(:disabled) {
    border-color: #b9c9ff;
    background: #eef3ff;
    color: #275dff;
  }

  .attachment-tools .tool-button:disabled {
    color: #abb3c2;
    cursor: default;
    opacity: 0.72;
  }

  .attachment-tools .tool-button svg {
    width: 18px;
  }

  .composer small {
    display: block;
    margin-top: 7px;
    color: #929bad;
    font-size: 0.62rem;
    text-align: right;
  }

  .send-error {
    margin-bottom: 8px;
    color: #b84150;
    font-size: 0.72rem;
  }

  .no-selection {
    grid-row: 1 / -1;
    background: #f9fafc;
  }

  .image-viewer {
    position: fixed;
    z-index: 20;
    inset: 0;
    padding: max(64px, calc(env(safe-area-inset-top) + 52px)) 24px max(24px, env(safe-area-inset-bottom));
    display: grid;
    place-items: center;
    background: rgba(15, 21, 35, 0.82);
    backdrop-filter: blur(12px);
  }

  .message-menu-overlay {
    position: fixed;
    z-index: 40;
    inset: 0;
    background: transparent;
  }

  .message-menu {
    position: fixed;
    left: clamp(12px, var(--message-menu-x), calc(100vw - 108px));
    top: clamp(12px, var(--message-menu-y), calc(100dvh - 56px));
    width: 96px;
    padding: 5px;
    border: 1px solid #dfe4ed;
    border-radius: 10px;
    background: rgba(255, 255, 255, 0.98);
    box-shadow: 0 12px 34px rgba(28, 39, 63, 0.2);
  }

  .message-menu button {
    width: 100%;
    min-height: 36px;
    border: 0;
    border-radius: 7px;
    background: transparent;
    color: #d33b4d;
    font: inherit;
    cursor: pointer;
  }

  .message-menu button:hover {
    background: #fff0f2;
  }

  .message-menu button:disabled {
    opacity: 0.55;
    cursor: default;
  }

  .profile-overlay {
    position: fixed;
    inset: 0;
    z-index: 30;
    padding: 24px;
    display: grid;
    place-items: center;
    overflow-y: auto;
    background: rgba(20, 28, 45, 0.24);
    backdrop-filter: blur(8px);
  }

  .profile-editor {
    width: min(390px, 100%);
    max-height: 100%;
    padding: 26px;
    overflow-y: auto;
    border: 1px solid #dce2ed;
    border-radius: 18px;
    background: #fff;
    box-shadow: 0 22px 60px rgba(23, 32, 51, 0.18);
  }

  .profile-editor h2 {
    margin: 0 0 18px;
    color: #273047;
    font-size: 1.1rem;
    letter-spacing: -0.025em;
  }

  .avatar-editor {
    margin-bottom: 20px;
    display: flex;
    align-items: center;
    gap: 14px;
  }

  .profile-avatar {
    position: relative;
    width: 68px;
    height: 68px;
    flex: none;
    display: grid;
    place-items: center;
    border: 1px solid #d5ddec;
    border-radius: 18px;
    background: #edf2ff;
    color: #4774ef;
    cursor: pointer;
  }

  .profile-avatar::before,
  .profile-avatar i {
    content: "";
    position: absolute;
    border: 2px solid currentColor;
    border-radius: 50%;
  }

  .profile-avatar::before {
    width: 38px;
    height: 38px;
  }

  .profile-avatar i {
    width: 9px;
    height: 9px;
    border: 0;
    background: currentColor;
  }

  .profile-avatar.has-image::before {
    display: none;
  }

  .avatar-editor > div {
    min-width: 0;
    flex: 1;
  }

  .avatar-editor button.avatar-action,
  .avatar-editor button.avatar-remove {
    min-height: 32px;
    padding: 0 11px;
    border: 1px solid #cbd6f1;
    border-radius: 8px;
    background: #f4f7ff;
    color: #275dff;
    font-size: 0.7rem;
    font-weight: 680;
    cursor: pointer;
  }

  .avatar-editor button.avatar-remove {
    margin-left: 5px;
    border-color: transparent;
    background: transparent;
    color: #8a94a7;
  }

  .avatar-editor button:disabled {
    cursor: default;
    opacity: 0.55;
  }

  .avatar-editor span {
    margin-top: 7px;
    display: block;
    color: #8a94a7;
    font-size: 0.62rem;
    line-height: 1.45;
  }

  .nickname-field {
    display: grid;
    gap: 7px;
  }

  .nickname-field > span {
    color: #4f5c74;
    font-size: 0.7rem;
    font-weight: 680;
  }

  .profile-editor input {
    width: 100%;
    height: 46px;
    padding: 0 13px;
    border: 1px solid #cfd7e5;
    border-radius: 11px;
    background: #f8f9fb;
    color: #172033;
    font-size: 0.88rem;
  }

  .profile-id,
  .profile-error {
    margin: 8px 2px 0;
    color: #8a94a7;
    font-size: 0.64rem;
  }

  .profile-error {
    color: #b84150;
  }

  .storage-row {
    margin-top: 14px;
    padding-top: 12px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    border-top: 1px solid #edf0f5;
    color: #68748b;
    font-size: 0.68rem;
  }

  .profile-editor .storage-row button {
    min-width: 0;
    min-height: 30px;
    padding: 0 9px;
    border: 0;
    background: transparent;
    color: #b84150;
    font-size: 0.68rem;
  }

  .profile-actions {
    margin-top: 22px;
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .profile-actions button {
    min-width: 76px;
    min-height: 38px;
    border: 1px solid #275dff;
    border-radius: 10px;
    background: #275dff;
    color: #fff;
    font-weight: 680;
    cursor: pointer;
  }

  .profile-actions button.secondary {
    border-color: #d9deea;
    background: #fff;
    color: #65708a;
  }

  .profile-actions button:disabled {
    cursor: default;
    opacity: 0.5;
  }

  .image-viewer-close {
    position: absolute;
    top: max(14px, env(safe-area-inset-top));
    right: max(14px, env(safe-area-inset-right));
    width: 36px;
    height: 36px;
    display: grid;
    place-items: center;
    border: 1px solid rgba(255, 255, 255, 0.15);
    border-radius: 18px;
    background: rgba(20, 26, 40, 0.72);
    color: #fff;
    cursor: pointer;
  }

  .image-viewer-close svg {
    width: 19px;
    fill: none;
    stroke: currentColor;
    stroke-linecap: round;
    stroke-width: 1.8;
  }

  .image-viewer > img {
    display: block;
    width: auto;
    height: auto;
    max-width: 100%;
    max-height: calc(100dvh - 96px - env(safe-area-inset-top) - env(safe-area-inset-bottom));
    border-radius: 10px;
    box-shadow: 0 24px 72px rgba(0, 0, 0, 0.35);
  }

  @keyframes pulse {
    0%,
    100% {
      box-shadow: 0 0 0 3px rgba(45, 170, 118, 0.1);
    }
    50% {
      box-shadow: 0 0 0 7px rgba(45, 170, 118, 0.02);
    }
  }

  @media (max-width: 760px) {
    :global(html) {
      width: 100%;
      height: 100%;
      overflow: hidden;
      overscroll-behavior: none;
      background: #fff;
    }

    :global(body) {
      width: 100%;
      height: 100%;
      overflow: hidden;
      overscroll-behavior: none;
      background: #fff;
    }

    main {
      position: fixed;
      top: var(--tossit-viewport-top, 0px);
      left: 0;
      width: 100%;
      height: var(--tossit-viewport-height, 100dvh);
      min-height: 0;
      margin: 0;
      display: block;
      border: 0;
      border-radius: 0;
      box-shadow: none;
    }

    .device-panel,
    .conversation-panel {
      width: 100%;
      height: 100%;
      display: none;
    }

    main:not(.peer-selected) .device-panel {
      display: grid;
    }

    main.peer-selected .conversation-panel {
      display: grid;
    }

    .device-panel {
      border-right: 0;
      grid-template-rows: auto minmax(0, 1fr);
      background: #fff;
    }

    .sidebar-header {
      height: auto;
      min-height: calc(58px + env(safe-area-inset-top));
      padding: env(safe-area-inset-top) 14px 0;
      border-bottom: 1px solid #e5e9f0;
      background: rgba(255, 255, 255, 0.96);
      backdrop-filter: saturate(150%) blur(18px);
    }

    .network-switch {
      max-width: calc(100% - 48px);
      background: #f5f7fa;
    }

    .back-button {
      width: 34px;
      height: 34px;
      margin-left: -7px;
      display: grid;
      place-items: center;
      border: 0;
      background: transparent;
      color: #35415a;
    }

    .back-button svg {
      width: 22px;
      fill: none;
      stroke: currentColor;
      stroke-linecap: round;
      stroke-linejoin: round;
      stroke-width: 1.8;
    }

    .device-content {
      padding: 10px 0 max(16px, env(safe-area-inset-bottom));
      background: #fff;
    }

    .device-placeholder {
      grid-row: 2;
    }

    .conversation-header {
      height: auto;
      min-height: calc(58px + env(safe-area-inset-top));
      padding: env(safe-area-inset-top) 14px 0;
      background: rgba(255, 255, 255, 0.96);
      backdrop-filter: saturate(150%) blur(18px);
    }

    .secure-state {
      font-size: 0;
    }

    .secure-state svg {
      width: 16px;
    }

    .message-list {
      width: calc(100% - 28px);
    }

    .bubble {
      max-width: 84%;
    }

    .bubble.attachment-bubble,
    .outgoing .bubble.attachment-bubble {
      width: min(320px, 84vw);
      max-width: 88%;
    }

    .trust-actions {
      width: 100%;
      flex-direction: column;
      gap: 5px;
    }

    .trust-actions button {
      width: 100%;
    }

    .trust-actions button.secondary {
      min-height: 32px;
      border-color: transparent;
      background: transparent;
    }

    .composer {
      padding: 8px 12px max(8px, env(safe-area-inset-bottom));
      background: rgba(255, 255, 255, 0.98);
    }

    :global(html.tossit-keyboard-visible) .composer {
      padding-bottom: 8px;
    }

    .composer-row {
      gap: 7px;
    }

    .attachment-tools {
      gap: 3px;
    }

    .attachment-tools .tool-button {
      width: 34px;
      height: 40px;
      border: 0;
      background: transparent;
    }

    .image-viewer {
      padding: max(64px, calc(env(safe-area-inset-top) + 52px)) 10px max(12px, env(safe-area-inset-bottom));
    }

    .image-viewer > img {
      border-radius: 6px;
    }

    .message-menu-overlay {
      padding: 0 10px max(10px, env(safe-area-inset-bottom));
      display: flex;
      align-items: flex-end;
      background: rgba(15, 21, 35, 0.16);
    }

    .message-menu {
      position: static;
      width: 100%;
      padding: 6px;
      border: 0;
      border-radius: 14px;
      box-shadow: 0 12px 36px rgba(28, 39, 63, 0.2);
    }

    .message-menu button {
      min-height: 48px;
      font-size: 0.92rem;
    }

    .composer small {
      display: none;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    *,
    *::before,
    *::after {
      scroll-behavior: auto !important;
      animation-duration: 0.01ms !important;
      animation-iteration-count: 1 !important;
    }
  }
</style>
