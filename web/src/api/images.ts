import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, ApiError, reloadIfAccountChanged, writeHeaders } from "./client";
import { keys } from "./keys";
import type { ApiErrorCode, UploadedImage } from "./types";

export const imageTypes = ["image/png", "image/jpeg", "image/gif", "image/webp"] as const;
export const maxImageBytes = 5 * 1024 * 1024;

/** Why this file can't be uploaded, or null when it can. Checked before sending so the person hears it right away. */
export function imageFileProblem(file: File): string | null {
  if (!(imageTypes as readonly string[]).includes(file.type)) return "Use a PNG, JPEG, GIF or WebP image.";
  if (file.size > maxImageBytes) return "That image is over 5 MB. Try a smaller or compressed one.";
  return null;
}

/** Sends the raw file with its Content-Type. XMLHttpRequest rather than fetch so upload progress can be shown. */
function upload(file: File, onProgress?: (fraction: number) => void): Promise<UploadedImage> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("POST", "/api/images");
    xhr.withCredentials = true;
    xhr.setRequestHeader("Content-Type", file.type);
    xhr.setRequestHeader("Accept", "application/json");
    for (const [k, v] of Object.entries(writeHeaders())) xhr.setRequestHeader(k, v);
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) onProgress?.(e.loaded / e.total);
    };
    xhr.onerror = () => reject(new ApiError(0, "network", "Can't reach the DumbMail server. Check your connection and try again."));
    xhr.onload = () => {
      let json: unknown;
      try {
        json = xhr.responseText ? JSON.parse(xhr.responseText) : null;
      } catch {
        json = null;
      }
      if (xhr.status >= 200 && xhr.status < 300) return resolve(json as UploadedImage);
      const err = (json as { error?: { code?: ApiErrorCode; message?: string } } | null)?.error;
      reloadIfAccountChanged(xhr.status, err?.code);
      reject(new ApiError(xhr.status, err?.code ?? "internal", err?.message ?? `Upload failed (${xhr.status})`));
    };
    xhr.send(file);
  });
}

export const imagesApi = {
  list: () => api<{ data: UploadedImage[] }>("GET", "/api/images"),
  upload,
  remove: (id: string) => api<unknown>("DELETE", `/api/images/${id}`),
};

/** Uploaded images, newest first. Pass `enabled: false` to wait until the picker opens. */
export function useImages(enabled = true) {
  return useQuery({
    queryKey: keys.images,
    queryFn: imagesApi.list,
    enabled,
    select: (r) => [...r.data].sort((a, b) => b.created_at.localeCompare(a.created_at)),
  });
}

export function useUploadImage() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ file, onProgress }: { file: File; onProgress?: (fraction: number) => void }) => imagesApi.upload(file, onProgress),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.images }),
  });
}

export function useDeleteImage() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: imagesApi.remove,
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.images }),
  });
}
