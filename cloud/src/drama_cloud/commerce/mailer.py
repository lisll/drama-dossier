"""发邮件。生产环境走 SMTP（STARTTLS 或 SSL）；开发时连本机的 Mailpit（明文，只允许本机）。"""

from __future__ import annotations

import logging
import smtplib
import ssl
from email.message import EmailMessage
from typing import Protocol

from .config import SmtpSettings

log = logging.getLogger(__name__)


class MailError(RuntimeError):
    """邮件没发出去。消息里不含收件人、正文和凭据。"""


class Mailer(Protocol):
    def send(self, to: str, subject: str, body: str) -> None: ...


class SmtpMailer:
    def __init__(self, settings: SmtpSettings):
        self.settings = settings

    def build(self, to: str, subject: str, body: str) -> EmailMessage:
        message = EmailMessage()
        message["From"] = self.settings.sender
        message["To"] = to
        message["Subject"] = subject
        # base64 传输正文：中文内容在任何 SMTP 服务器上都不会被改坏。
        message.set_content(body, cte="base64")
        return message

    def send(self, to: str, subject: str, body: str) -> None:
        settings = self.settings
        message = self.build(to, subject, body)
        context = ssl.create_default_context()
        try:
            if settings.security == "ssl":
                client = smtplib.SMTP_SSL(
                    settings.host, settings.port, timeout=settings.timeout_seconds, context=context
                )
            else:
                client = smtplib.SMTP(settings.host, settings.port, timeout=settings.timeout_seconds)
            with client:
                if settings.security == "starttls":
                    client.starttls(context=context)
                if settings.username:
                    client.login(settings.username, settings.password or "")
                client.send_message(message)
        except (smtplib.SMTPException, OSError) as error:
            log.warning("验证码邮件发送失败：%s", type(error).__name__)
            raise MailError(f"邮件服务暂时不可用（{type(error).__name__}）") from error
