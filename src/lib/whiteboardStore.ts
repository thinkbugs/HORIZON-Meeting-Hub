import type { PlaitElement, PlaitTheme, Viewport } from "@plait/core";

export interface WhiteboardDocument {
  children: PlaitElement[];
  viewport?: Viewport;
  theme?: PlaitTheme;
  savedAt?: number;
}

const DB_NAME = "horizon-whiteboard";
const DB_VERSION = 2;
const STORE_NAME = "documents";
const DOCUMENT_KEY = "main";

let dbPromise: Promise<IDBDatabase | null> | null = null;

function openDatabase(): Promise<IDBDatabase | null> {
  if (dbPromise) return dbPromise;
  if (typeof indexedDB === "undefined") {
    dbPromise = Promise.resolve(null);
    return dbPromise;
  }
  dbPromise = new Promise((resolve) => {
    const request = indexedDB.open(DB_NAME, DB_VERSION);
    request.onupgradeneeded = () => {
      const database = request.result;
      if (!database.objectStoreNames.contains(STORE_NAME)) {
        database.createObjectStore(STORE_NAME);
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => resolve(null);
    request.onblocked = () => resolve(null);
  });
  return dbPromise;
}

export async function loadWhiteboardDocument(): Promise<WhiteboardDocument | null> {
  const database = await openDatabase();
  if (!database) return null;
  return new Promise((resolve) => {
    try {
      const transaction = database.transaction(STORE_NAME, "readonly");
      const request = transaction.objectStore(STORE_NAME).get(DOCUMENT_KEY);
      request.onsuccess = () => {
        const value = request.result as WhiteboardDocument | undefined;
        resolve(value && Array.isArray(value.children) ? value : null);
      };
      request.onerror = () => resolve(null);
    } catch {
      resolve(null);
    }
  });
}

export async function saveWhiteboardDocument(document: WhiteboardDocument): Promise<void> {
  const database = await openDatabase();
  if (!database) return;
  const payload: WhiteboardDocument = { ...document, savedAt: Date.now() };
  await new Promise<void>((resolve, reject) => {
    try {
      const transaction = database.transaction(STORE_NAME, "readwrite");
      transaction.objectStore(STORE_NAME).put(payload, DOCUMENT_KEY);
      transaction.oncomplete = () => resolve();
      transaction.onerror = () => reject(transaction.error);
      transaction.onabort = () => reject(transaction.error);
    } catch (reason) {
      reject(reason);
    }
  });
}
