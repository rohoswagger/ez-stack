"""Setup script for ez-stack Python wheel distribution."""

from setuptools import setup, find_packages

setup(
    packages=find_packages(),
    package_data={
        "ez_stack": ["bin/*"],
    },
    include_package_data=True,
)
