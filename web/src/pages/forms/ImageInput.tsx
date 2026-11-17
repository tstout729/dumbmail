import { useRef, useState } from "react";
import { ImageIcon, Images, Trash2, Upload, X } from "lucide-react";
import { imageFileProblem, imageTypes, useDeleteImage, useImages, useUploadImage, type UploadedImage } from "../../api";
import { Button, ConfirmDialog, IconButton, Input, Modal, Skeleton, cx } from "../../components/ui";
import { toast } from "../../lib/toast";
import { urlError } from "./editorShared";

type ImageInputProps = {
  value: string;
  /** Shown while empty, e.g. version A's image on a B tab. */
  placeholder?: string;
  onChange: (url: string) => void;
  "aria-label"?: string;
};

/** Image URL with Upload and a library of past uploads. */
export function ImageInput({ value, placeholder, onChange, "aria-label": ariaLabel }: ImageInputProps) {
  const fileRef = useRef<HTMLInputElement>(null);
  const upload = useUploadImage();
  const [progress, setProgress] = useState<number | null>(null);
  const [library, setLibrary] = useState(false);
  const shown = value || placeholder || "";
  const error = urlError(value);

  const pick = (file: File | undefined) => {
    if (!file) return;
    const problem = imageFileProblem(file);
    if (problem) {
      toast.error(problem);
      return;
    }
    setProgress(0);
    upload.mutate(
      { file, onProgress: setProgress },
      {
        onSuccess: (img) => {
          onChange(img.url);
          toast.success("Image uploaded");
        },
        onSettled: () => setProgress(null),
      },
    );
  };

  return (
    <div className="space-y-2">
      <div className="flex gap-3">
        <div className="flex size-16 shrink-0 items-center justify-center overflow-hidden rounded-md border border-zinc-200 bg-zinc-50 text-zinc-400">
          {shown && !error ? <img src={shown} alt="" className={cx("size-full object-cover", !value && "opacity-50")} /> : <ImageIcon className="size-5" />}
        </div>
        <div className="min-w-0 flex-1 space-y-1.5">
          <div className="flex gap-1.5">
            <Input
              value={value}
              placeholder={placeholder || "https://…/photo.jpg"}
              onChange={(e) => onChange(e.target.value.trim())}
              aria-label={ariaLabel ?? "Image URL"}
              className="min-w-0 flex-1"
            />
            {value ? (
              <IconButton label="Remove image" onClick={() => onChange("")}>
                <X className="size-4" />
              </IconButton>
            ) : null}
          </div>
          <div className="flex flex-wrap gap-1.5">
            <Button size="sm" icon={<Upload className="size-3.5" />} loading={upload.isPending} onClick={() => fileRef.current?.click()}>
              {progress !== null ? `Uploading ${Math.round(progress * 100)}%` : "Upload"}
            </Button>
            <Button size="sm" variant="ghost" icon={<Images className="size-3.5" />} onClick={() => setLibrary(true)}>
              Your images
            </Button>
          </div>
        </div>
      </div>
      {error ? <p className="text-xs text-red-600">{error}</p> : <p className="text-xs text-zinc-500">PNG, JPEG, GIF or WebP, up to 5 MB.</p>}
      <input
        ref={fileRef}
        type="file"
        accept={imageTypes.join(",")}
        className="sr-only"
        tabIndex={-1}
        onChange={(e) => {
          pick(e.target.files?.[0]);
          e.target.value = "";
        }}
      />
      {library ? (
        <ImageLibrary
          current={value}
          onPick={(url) => {
            onChange(url);
            setLibrary(false);
          }}
          onClose={() => setLibrary(false)}
        />
      ) : null}
    </div>
  );
}

function ImageLibrary({ current, onPick, onClose }: { current: string; onPick: (url: string) => void; onClose: () => void }) {
  const images = useImages();
  const remove = useDeleteImage();
  const [deleting, setDeleting] = useState<UploadedImage | null>(null);
  return (
    <Modal title="Your images" description="Images you've uploaded. Pick one to use it." onClose={onClose} size="lg">
      {images.isPending ? (
        <div className="grid grid-cols-3 gap-3 sm:grid-cols-4">
          {[0, 1, 2, 3].map((i) => (
            <Skeleton key={i} className="aspect-square" />
          ))}
        </div>
      ) : images.error ? (
        <p className="text-sm text-red-600">{images.error.message}</p>
      ) : (images.data ?? []).length === 0 ? (
        <p className="py-8 text-center text-sm text-zinc-500">No images yet. Use Upload to add one.</p>
      ) : (
        <div className="grid max-h-[60vh] grid-cols-3 gap-3 overflow-y-auto sm:grid-cols-4">
          {(images.data ?? []).map((img) => (
            <div key={img.id} className="group relative">
              <button
                type="button"
                onClick={() => onPick(img.url)}
                className={cx(
                  "block aspect-square w-full overflow-hidden rounded-md border bg-zinc-50",
                  img.url === current ? "border-accent-600 ring-2 ring-accent-600/30" : "border-zinc-200 hover:border-zinc-400",
                )}
                aria-label="Use this image"
              >
                <img src={img.url} alt="" loading="lazy" className="size-full object-cover" />
              </button>
              <button
                type="button"
                onClick={() => setDeleting(img)}
                className="absolute right-1 top-1 rounded bg-white/90 p-1 text-zinc-500 opacity-0 shadow-sm transition-opacity hover:text-red-700 focus:opacity-100 group-hover:opacity-100"
                aria-label="Delete image"
                title="Delete image"
              >
                <Trash2 className="size-3.5" />
              </button>
            </div>
          ))}
        </div>
      )}
      {deleting ? (
        <ConfirmDialog
          title="Delete this image?"
          body="Anything still using it, like a form or an email, shows a broken image."
          confirmLabel="Delete image"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(deleting.id, { onSuccess: () => setDeleting(null) })}
          onClose={() => setDeleting(null)}
        />
      ) : null}
    </Modal>
  );
}
