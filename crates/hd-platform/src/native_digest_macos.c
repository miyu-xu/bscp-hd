#include <CommonCrypto/CommonDigest.h>
#include <errno.h>
#include <unistd.h>

int hd_commoncrypto_sha256_fd(int fd, unsigned char digest[CC_SHA256_DIGEST_LENGTH],
                              int *error_number) {
  CC_SHA256_CTX context;
  unsigned char buffer[256 * 1024];

  if (error_number != NULL) {
    *error_number = 0;
  }
  if (fd < 0 || digest == NULL || CC_SHA256_Init(&context) != 1) {
    if (error_number != NULL) {
      *error_number = EINVAL;
    }
    return 0;
  }

  for (;;) {
    ssize_t count = read(fd, buffer, sizeof(buffer));
    if (count == 0) {
      break;
    }
    if (count < 0) {
      if (errno == EINTR) {
        continue;
      }
      if (error_number != NULL) {
        *error_number = errno;
      }
      return 0;
    }
    if (CC_SHA256_Update(&context, buffer, (CC_LONG)count) != 1) {
      if (error_number != NULL) {
        *error_number = EIO;
      }
      return 0;
    }
  }

  if (CC_SHA256_Final(digest, &context) != 1) {
    if (error_number != NULL) {
      *error_number = EIO;
    }
    return 0;
  }
  return 1;
}
