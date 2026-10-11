// Opens a cloud move's job in the cloud sheet from outside React (the "Review" toast of a
// bring-back). The keyboard layer registers the opener while the shell is mounted.

let opener: ((host: string, job: string) => void) | null = null;

export function setCloudJobOpener(f: ((host: string, job: string) => void) | null): void {
  opener = f;
}

/** Show a cloud move's job (a bring-back waiting for review) in the cloud sheet. */
export function openCloudJob(host: string, job: string): void {
  opener?.(host, job);
}
